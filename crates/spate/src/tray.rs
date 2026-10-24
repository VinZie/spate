//! Notification-area (system tray) icon with a small menu.

use std::sync::mpsc::Sender;

use eframe::egui;

use crate::app::UiEvent;

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::app::TrayAction;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        icon: TrayIcon,
        last_tooltip: std::cell::RefCell<String>,
    }

    impl Tray {
        pub fn new(ctx: &egui::Context, tx: Sender<UiEvent>) -> Option<Self> {
            let menu = Menu::new();
            let items = [
                MenuItem::with_id("show", "Show Spate", true, None),
                MenuItem::with_id("add", "Add download…", true, None),
                MenuItem::with_id("pause", "Pause all", true, None),
                MenuItem::with_id("resume", "Resume all", true, None),
            ];
            for i in &items {
                menu.append(i).ok()?;
            }
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&MenuItem::with_id("exit", "Exit", true, None)).ok()?;

            let art = crate::icons::app_icon(32);
            let icon = tray_icon::Icon::from_rgba(art.rgba, art.width, art.height).ok()?;
            let tray = TrayIconBuilder::new()
                .with_id("spate")
                .with_tooltip("Spate")
                .with_icon(icon)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .ok()?;

            let (tx1, c1) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let action = match e.id.0.as_str() {
                    "show" => TrayAction::Show,
                    "add" => TrayAction::AddUrl,
                    "pause" => TrayAction::PauseAll,
                    "resume" => TrayAction::ResumeAll,
                    "exit" => TrayAction::Exit,
                    _ => return,
                };
                let _ = tx1.send(UiEvent::Tray(action));
                c1.request_repaint();
            }));
            let c2 = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                let show = matches!(
                    e,
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
                );
                if show {
                    let _ = tx.send(UiEvent::Tray(TrayAction::Show));
                    c2.request_repaint();
                }
            }));
            Some(Self { icon: tray, last_tooltip: std::cell::RefCell::new("Spate".into()) })
        }

        pub fn set_tooltip(&self, text: &str) {
            let mut last = self.last_tooltip.borrow_mut();
            if *last != text {
                let _ = self.icon.set_tooltip(Some(text));
                *last = text.to_string();
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub struct Tray;

    impl Tray {
        pub fn new(_ctx: &egui::Context, _tx: Sender<UiEvent>) -> Option<Self> {
            None
        }

        pub fn set_tooltip(&self, _text: &str) {}
    }
}

pub use imp::Tray;
