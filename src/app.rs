// SPDX-License-Identifier: GPL-3.0-only

use crate::backend::{self, ExtForeignToplevelHandleV1, ToplevelInfo};
use clap::Parser;
use cosmic::{
    app::{Application, Core, CosmicFlags, Settings, Task},
    cctk::sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer},
    dbus_activation::Details,
    iced::{
        self, Alignment, Border, Color, Length, Subscription,
        keyboard::{
            self,
            key::{Key, Named},
        },
        platform_specific::shell::commands::layer_surface::{
            destroy_layer_surface, get_layer_surface,
        },
        widget::Row,
        window,
    },
    iced::runtime::platform_specific::wayland::layer_surface::SctkLayerSurfaceSettings,
    theme, widget, Element,
};
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;

pub fn run() -> iced::Result {
    let args = Args::parse();
    cosmic::app::run_single_instance::<App>(
        Settings::default()
            .antialiasing(true)
            .client_decorations(true)
            .no_main_window(true)
            .exit_on_close(false),
        args,
    )
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[clap(subcommand)]
    pub subcommand: Option<AltTabAction>,
}

#[derive(Debug, Serialize, Deserialize, Clone, clap::Subcommand)]
pub enum AltTabAction {
    /// Cycle forward through windows
    AltTab,
    /// Cycle backward through windows
    ShiftAltTab,
}

impl Display for AltTabAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::ser::to_string(self).unwrap())
    }
}

impl FromStr for AltTabAction {
    type Err = serde_json::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::de::from_str(s)
    }
}

impl CosmicFlags for Args {
    type SubCommand = AltTabAction;
    type Args = Vec<String>;
    fn action(&self) -> Option<&AltTabAction> {
        self.subcommand.as_ref()
    }
}

#[derive(Debug, Clone)]
struct Window {
    handle: ExtForeignToplevelHandleV1,
    info: ToplevelInfo,
    thumbnail: Option<widget::image::Handle>,
}

#[derive(Default)]
struct App {
    core: Core,
    windows: Vec<Window>,
    selected: usize,
    layer_surface: Option<window::Id>,
    cmd_sender: Option<calloop::channel::Sender<backend::Cmd>>,
    alt_was_held: bool,
    visible: bool,
    fade_phase: FadePhase,
    fade_start: Option<std::time::Instant>,
    pending_close: Option<window::Id>, // destroy this surface after fade-out completes
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
enum FadePhase {
    #[default]
    Idle,
    FadingIn,
    Open,
    FadingOut,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Next,
    Prev,
    ModifiersChanged(bool),
    Escape,
    Activate(usize),
    Backend(backend::Event),
    Show,
    Hide,
    FadeTick,
    Surface(cosmic::surface::Action),
}

const FADE_DURATION_MS: u64 = 250;

impl Application for App {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = Args;
    type Message = Msg;
    const APP_ID: &'static str = "com.github.jibsta210.CosmicAltSwitcher";

    fn init(core: Core, flags: Args) -> (Self, Task<Msg>) {
        let mut app = App {
            core,
            windows: Vec::new(),
            selected: 0,
            layer_surface: None,
            cmd_sender: None,
            alt_was_held: true,
            visible: false,
            fade_phase: FadePhase::Idle,
            fade_start: None,
            pending_close: None,
        };
        // If launched with a subcommand (first invocation case), immediately show.
        // The Next/Prev cycle happens once we have windows enumerated.
        let initial_task = if flags.subcommand.is_some() {
            app.update(Msg::Show)
        } else {
            Task::none()
        };
        (app, initial_task)
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn dbus_activation(
        &mut self,
        msg: cosmic::dbus_activation::Message,
    ) -> Task<Msg> {
        match msg.msg {
            Details::Activate => self.update(Msg::Show),
            Details::ActivateAction { action, .. } => {
                let Ok(cmd) = AltTabAction::from_str(&action) else {
                    return Task::none();
                };
                match cmd {
                    AltTabAction::AltTab => {
                        if !self.visible {
                            self.update(Msg::Show).chain(self.update(Msg::Next))
                        } else {
                            self.update(Msg::Next)
                        }
                    }
                    AltTabAction::ShiftAltTab => {
                        if !self.visible {
                            self.update(Msg::Show).chain(self.update(Msg::Prev))
                        } else {
                            self.update(Msg::Prev)
                        }
                    }
                }
            }
            Details::Open { .. } => Task::none(),
        }
    }

    fn update(&mut self, message: Msg) -> Task<Msg> {
        match message {
            Msg::Show => {
                if self.visible {
                    return Task::none();
                }
                self.visible = true;
                self.alt_was_held = true;
                self.fade_phase = FadePhase::FadingIn;
                self.fade_start = Some(std::time::Instant::now());
                // Request fresh captures for all windows — screenshots may be stale.
                if let Some(tx) = &self.cmd_sender {
                    for w in &self.windows {
                        let _ = tx.send(backend::Cmd::CaptureToplevel(w.handle.clone()));
                    }
                }
                let id = window::Id::unique();
                self.layer_surface = Some(id);
                return get_layer_surface(SctkLayerSurfaceSettings {
                    id,
                    keyboard_interactivity: KeyboardInteractivity::Exclusive,
                    namespace: "cosmic-altswitcher".into(),
                    layer: Layer::Overlay,
                    size: Some((None, None)),
                    anchor: Anchor::all(),
                    ..Default::default()
                });
            }
            Msg::Hide => {
                // Start fade-out instead of destroying immediately
                if self.visible && self.fade_phase != FadePhase::FadingOut {
                    self.fade_phase = FadePhase::FadingOut;
                    self.fade_start = Some(std::time::Instant::now());
                    self.pending_close = self.layer_surface;
                }
            }
            Msg::FadeTick => {
                if let Some(start) = self.fade_start {
                    let elapsed = start.elapsed().as_millis() as u64;
                    if elapsed >= FADE_DURATION_MS {
                        match self.fade_phase {
                            FadePhase::FadingIn => {
                                self.fade_phase = FadePhase::Open;
                                self.fade_start = None;
                            }
                            FadePhase::FadingOut => {
                                // Destroy the layer surface now
                                self.fade_phase = FadePhase::Idle;
                                self.fade_start = None;
                                self.visible = false;
                                self.layer_surface = None;
                                if let Some(id) = self.pending_close.take() {
                                    return destroy_layer_surface(id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Msg::Next => {
                if !self.windows.is_empty() {
                    self.selected = (self.selected + 1) % self.windows.len();
                }
            }
            Msg::Prev => {
                if !self.windows.is_empty() {
                    self.selected = if self.selected == 0 {
                        self.windows.len() - 1
                    } else {
                        self.selected - 1
                    };
                }
            }
            Msg::ModifiersChanged(alt_held) => {
                if self.visible && self.alt_was_held && !alt_held {
                    return self.activate_selected_and_close();
                }
                self.alt_was_held = alt_held;
            }
            Msg::Escape => {
                return self.update(Msg::Hide);
            }
            Msg::Activate(i) => {
                self.selected = i;
                return self.activate_selected_and_close();
            }
            Msg::Backend(ev) => match ev {
                backend::Event::CmdSender(tx) => {
                    self.cmd_sender = Some(tx);
                }
                backend::Event::NewToplevel(handle, info) => {
                    self.windows.push(Window {
                        handle,
                        info,
                        thumbnail: None,
                    });
                    if self.windows.len() > 1 && self.selected == 0 {
                        self.selected = 1;
                    }
                }
                backend::Event::ToplevelCapture(handle, pixels, w, h) => {
                    if let Some(win) = self.windows.iter_mut().find(|win| win.handle == handle) {
                        // iced expects an owned Vec<u8>; unwrap Arc or clone
                        let bytes = Arc::try_unwrap(pixels).unwrap_or_else(|a| (*a).clone());
                        win.thumbnail = Some(widget::image::Handle::from_rgba(w, h, bytes));
                    }
                }
                backend::Event::UpdateToplevel(handle, info) => {
                    if let Some(w) = self.windows.iter_mut().find(|w| w.handle == handle) {
                        w.info = info;
                    }
                }
                backend::Event::CloseToplevel(handle) => {
                    self.windows.retain(|w| w.handle != handle);
                    if self.selected >= self.windows.len() && !self.windows.is_empty() {
                        self.selected = self.windows.len() - 1;
                    }
                }
            },
            Msg::Surface(_) => {}
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Msg> {
        let mut subs: Vec<Subscription<Msg>> = Vec::new();

        // Fade animation tick — only active during fade
        if matches!(self.fade_phase, FadePhase::FadingIn | FadePhase::FadingOut) {
            use iced::futures::StreamExt;
            use std::hash::Hash;
            #[derive(Clone)]
            struct FadeId;
            impl Hash for FadeId {
                fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
            }
            subs.push(Subscription::run_with(FadeId, |_| {
                let stream = iced::futures::stream::unfold((), |_| async {
                    tokio::time::sleep(std::time::Duration::from_millis(8)).await;
                    Some((Msg::FadeTick, ()))
                });
                stream.boxed()
            }));
        }

        subs.push(
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::Tab),
                    modifiers,
                    ..
                }) => {
                    if modifiers.shift() {
                        Some(Msg::Prev)
                    } else {
                        Some(Msg::Next)
                    }
                }
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::ArrowLeft),
                    ..
                }) => Some(Msg::Prev),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::ArrowRight),
                    ..
                }) => Some(Msg::Next),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::Enter),
                    ..
                }) => Some(Msg::ModifiersChanged(false)),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::Escape),
                    ..
                }) => Some(Msg::Escape),
                iced::Event::Keyboard(keyboard::Event::ModifiersChanged(mods)) => {
                    Some(Msg::ModifiersChanged(mods.alt() || mods.logo()))
                }
                _ => None,
            }),
        );
        subs.push(backend::subscription().map(Msg::Backend));
        Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Msg> {
        widget::text("").into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Msg> {
        let alpha = self.fade_alpha();
        let selected = self.selected;

        let thumbnails: Vec<Element<'_, Msg>> = self
            .windows
            .iter()
            .enumerate()
            .map(|(i, w)| {
                // Distance-from-selected attenuation: selected = 1.0,
                // each step away loses 8% (down to min 0.75).
                let dist = (i as i32 - selected as i32).unsigned_abs() as f32;
                let distance_scale = (1.0 - dist * 0.08).max(0.75);
                window_thumbnail(w, i == selected, i, alpha, distance_scale)
            })
            .collect();

        if thumbnails.is_empty() {
            return widget::container(widget::text::body("No windows"))
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into();
        }

        let strip = Row::with_children(thumbnails)
            .spacing(16)
            .align_y(Alignment::Center);

        widget::container(strip)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .class(theme::Container::custom(move |theme| {
                let mut bg: Color = theme.cosmic().background.base.into();
                bg.a = 0.6 * alpha;
                iced::widget::container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                }
            }))
            .into()
    }
}

impl App {
    fn fade_alpha(&self) -> f32 {
        match self.fade_phase {
            FadePhase::Idle => 0.0,
            FadePhase::Open => 1.0,
            FadePhase::FadingIn => {
                let elapsed = self.fade_start.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                (elapsed as f32 / FADE_DURATION_MS as f32).min(1.0)
            }
            FadePhase::FadingOut => {
                let elapsed = self.fade_start.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                (1.0 - (elapsed as f32 / FADE_DURATION_MS as f32)).max(0.0)
            }
        }
    }

    fn activate_selected_and_close(&mut self) -> Task<Msg> {
        if let (Some(w), Some(tx)) = (self.windows.get(self.selected), &self.cmd_sender) {
            let _ = tx.send(backend::Cmd::ActivateToplevel(w.handle.clone()));
        }
        self.update(Msg::Hide)
    }
}

fn window_thumbnail<'a>(
    w: &'a Window,
    selected: bool,
    index: usize,
    alpha: f32,
    distance_scale: f32,
) -> Element<'a, Msg> {
    // 35% bigger than the original 300x200 baseline = 405x270
    let base_w: f32 = 405.0;
    let base_h: f32 = 270.0;
    let selection_scale: f32 = if selected { 1.10 } else { 1.0 };
    let scale = selection_scale * distance_scale;
    let width = base_w * scale;
    let height = base_h * scale;

    // Inner contents: fixed-size column that fills the card
    let truncated_title = if w.info.title.len() > 32 {
        format!("{}…", &w.info.title.chars().take(31).collect::<String>())
    } else {
        w.info.title.clone()
    };

    // Preview area: use screenshot if available, otherwise fall back to app icon.
    // Explicit fixed height so iced doesn't collapse it inside the card.
    let preview_h: f32 = height - 48.0; // leave room for title+padding

    let screenshot: Element<'_, Msg> = if let Some(img) = &w.thumbnail {
        widget::image(img.clone())
            .content_fit(iced::ContentFit::Contain)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        widget::icon::from_name(w.info.app_id.clone()).size(64).into()
    };

    // App icon badge overlaid in the bottom-left corner of the screenshot
    let icon_badge: Element<'_, Msg> = widget::container(
        widget::icon::from_name(w.info.app_id.clone()).size(36),
    )
    .padding(6)
    .class(theme::Container::custom(|theme| {
        let cosmic = theme.cosmic();
        let mut bg: Color = cosmic.background.component.base.into();
        bg.a = 0.85;
        iced::widget::container::Style {
            background: Some(bg.into()),
            border: Border {
                radius: cosmic.radius_s().into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            ..Default::default()
        }
    }))
    .into();

    // Layer the icon badge on top of the screenshot using stack
    let preview: Element<'_, Msg> = widget::container(
        iced::widget::stack![
            widget::container(screenshot)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
            widget::container(icon_badge)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Left)
                .align_y(iced::alignment::Vertical::Bottom)
                .padding(8),
        ],
    )
    .width(Length::Fill)
    .height(Length::Fixed(preview_h))
    .into();

    let contents = iced::widget::Column::new()
        .push(preview)
        .push(
            widget::container(widget::text::body(truncated_title))
                .width(Length::Fill)
                .center_x(Length::Fill),
        )
        .spacing(4)
        .align_x(Alignment::Center)
        .width(Length::Fill);

    // Wrap contents in a fixed-size container with styled background/border
    let card = widget::container(contents)
        .width(Length::Fixed(width))
        .height(Length::Fixed(height))
        .padding(4)
        .class(theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            let mut bg: Color = cosmic.background.component.base.into();
            bg.a *= alpha;
            let mut border_color: Color = if selected {
                cosmic.accent.base.into()
            } else {
                cosmic.bg_divider().into()
            };
            border_color.a *= alpha;
            iced::widget::container::Style {
                background: Some(bg.into()),
                border: Border {
                    color: border_color,
                    width: if selected { 3.0 } else { 1.0 },
                    radius: cosmic.radius_s().into(),
                },
                ..Default::default()
            }
        }));

    // Wrap in another container to enforce outer bounds even in a Row
    widget::container(
        widget::button::custom(card)
            .on_press(Msg::Activate(index))
            .class(theme::Button::Transparent),
    )
    .width(Length::Fixed(width))
    .height(Length::Fixed(height))
    .into()
}
