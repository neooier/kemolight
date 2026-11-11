use std::{
    sync::{
        mpsc::{self, Receiver},
        LazyLock,
    },
    thread,
    time::Duration,
};

use crate::{
    app::{AppController, DisplayState},
    brightness::DisplayId,
    logo,
    profiles::ProfileSlot,
};

use super::TrayHandle;

const TRAY_UPDATE_DEBOUNCE: Duration = Duration::from_millis(150);
const LOGO_ICON_SIZE: i32 = 32;

#[derive(Clone)]
struct BrightnessTray {
    controller: AppController,
}

impl BrightnessTray {
    fn display_menu(&self, display: DisplayState) -> ksni::MenuItem<Self> {
        use ksni::menu::*;

        let display_id = display.info.id.clone();
        let current = display
            .info
            .current
            .map(|value| format!("{value}%"))
            .unwrap_or_else(|| "unknown".to_string());
        let label = format!("{} ({current})", display.info.name);
        let enabled = display.info.supported;

        SubMenu {
            label,
            enabled: true,
            submenu: vec![
                self.adjust_item("Brightness +5", display_id.clone(), 1, enabled),
                self.adjust_item("Brightness -5", display_id.clone(), -1, enabled),
                MenuItem::Separator,
                self.set_item("Set 25%", display_id.clone(), 25, enabled),
                self.set_item("Set 50%", display_id.clone(), 50, enabled),
                self.set_item("Set 75%", display_id.clone(), 75, enabled),
                self.set_item("Set 100%", display_id, 100, enabled),
            ],
            ..Default::default()
        }
        .into()
    }

    fn adjust_item(
        &self,
        label: &'static str,
        display_id: DisplayId,
        delta: i32,
        enabled: bool,
    ) -> ksni::MenuItem<Self> {
        use ksni::menu::StandardItem;

        StandardItem {
            label: label.to_string(),
            enabled,
            activate: Box::new(move |this: &mut Self| {
                this.controller.adjust_brightness(display_id.clone(), delta);
            }),
            ..Default::default()
        }
        .into()
    }

    fn set_item(
        &self,
        label: &'static str,
        display_id: DisplayId,
        value: u32,
        enabled: bool,
    ) -> ksni::MenuItem<Self> {
        use ksni::menu::StandardItem;

        StandardItem {
            label: label.to_string(),
            enabled,
            activate: Box::new(move |this: &mut Self| {
                this.controller.set_brightness(display_id.clone(), value);
            }),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for BrightnessTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "kemolight".to_string()
    }

    fn title(&self) -> String {
        "Brightness Controller".to_string()
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static ICON: LazyLock<ksni::Icon> = LazyLock::new(|| logo::tray_icon_argb(LOGO_ICON_SIZE));
        vec![ICON.clone()]
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.controller.request_open_window();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let snapshot = self.controller.snapshot();
        let mut items = vec![
            StandardItem {
                label: "Open Window".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.controller.request_open_window();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Refresh Displays".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.controller.refresh();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];

        for slot in ProfileSlot::ALL {
            items.push(
                StandardItem {
                    label: format!("{} Profile", slot.label()),
                    enabled: self.controller.can_apply_profile(slot),
                    activate: Box::new(move |this: &mut Self| this.controller.apply_profile(slot)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);

        if let Some(error) = &snapshot.profile_error {
            items.push(
                StandardItem {
                    label: error.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }

        if let Some(error) = snapshot.last_error {
            items.push(
                StandardItem {
                    label: error,
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }

        if snapshot.displays.is_empty() {
            items.push(
                StandardItem {
                    label: "No displays detected".to_string(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            for display in snapshot.displays {
                items.push(self.display_menu(display));
            }
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.controller.request_quit();
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

pub fn start_tray(controller: AppController) -> TrayHandle {
    use ksni::blocking::TrayMethods;

    let tray = BrightnessTray { controller };
    match tray.spawn() {
        Ok(handle) => {
            let (update_tx, update_rx) = mpsc::sync_channel(1);
            spawn_tray_updater(update_rx, handle.clone());

            TrayHandle::available(Some(update_tx), move || {
                handle.shutdown().wait();
            })
        }
        Err(err) => {
            eprintln!("Tray unavailable, falling back to window-only mode: {err}");
            TrayHandle::unavailable()
        }
    }
}

fn spawn_tray_updater(update_rx: Receiver<()>, handle: ksni::blocking::Handle<BrightnessTray>) {
    thread::spawn(move || {
        while update_rx.recv().is_ok() {
            thread::sleep(TRAY_UPDATE_DEBOUNCE);
            while update_rx.try_recv().is_ok() {}

            if handle.update(|_| ()).is_none() {
                break;
            }
        }
    });
}
