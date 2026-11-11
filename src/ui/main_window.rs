use std::time::Duration;

use eframe::egui::{self, RichText, Slider, ViewportCommand};

use crate::{
    app::{AppCommand, AppController, DisplayState},
    brightness::DisplayId,
    profiles::ProfileSlot,
};

const PENDING_REPAINT_INTERVAL: Duration = Duration::from_millis(100);
const DISPLAY_NAME_WIDTH: f32 = 170.0;
const PERCENT_WIDTH: f32 = 48.0;
const SLIDER_WIDTH: f32 = 210.0;
const STEP_BUTTON_SIZE: f32 = 24.0;

pub struct MainWindow {
    controller: AppController,
    visible: bool,
    quitting: bool,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    startup_setting: Option<StartupSetting>,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
struct StartupSetting {
    enabled: Option<bool>,
    error: Option<String>,
}

impl MainWindow {
    pub fn new(controller: AppController, visible: bool) -> Self {
        Self {
            controller,
            visible,
            quitting: false,
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            startup_setting: None,
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if !close_requested {
            return;
        }

        if self.quitting || !self.controller.tray_available() {
            self.quit(ctx);
        } else {
            self.hide_to_tray(ctx);
        }
    }

    fn handle_app_commands(&mut self, ctx: &egui::Context) {
        if self.quitting {
            return;
        }
        for command in self.controller.drain_app_commands() {
            match command {
                AppCommand::OpenWindow => self.show_window(ctx),
                AppCommand::HideWindow => self.hide_to_tray(ctx),
                AppCommand::Quit => self.quit(ctx),
                #[cfg(target_os = "windows")]
                AppCommand::UiClosed(_) => {}
            }
            if self.quitting {
                break;
            }
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        if !self.visible {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        }
        self.visible = true;
        #[cfg(target_os = "windows")]
        self.controller.window_shown();
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        if !self.controller.tray_available() {
            self.quit(ctx);
            return;
        }

        self.visible = false;
        self.controller.remember_window_geometry(ctx);

        // Return from run_native so the window, renderer, and egui caches can
        // be dropped. The tray session recreates the UI when it is opened.
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        if self.quitting {
            return;
        }

        self.quitting = true;
        self.controller.shutdown();
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn schedule_repaint(&self, ctx: &egui::Context) {
        if self
            .controller
            .snapshot()
            .displays
            .iter()
            .any(|display| display.pending)
        {
            ctx.request_repaint_after(PENDING_REPAINT_INTERVAL);
        }
    }

    fn render_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.controller.refresh();
                ui.ctx().request_repaint();
            }

            ui.menu_button("Profiles", |ui| {
                let snapshot = self.controller.snapshot();
                ui.set_min_width(270.0);
                for slot in ProfileSlot::ALL {
                    ui.strong(slot.label());
                    let saved = snapshot.profiles.get(slot);
                    if saved.is_empty() {
                        ui.weak("No brightness saved yet");
                    } else {
                        for entry in saved.values() {
                            ui.label(format!("{}: {}%", entry.name, entry.brightness));
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                self.controller.can_save_profile(),
                                egui::Button::new("Save current"),
                            )
                            .clicked()
                        {
                            self.controller.save_profile(slot);
                            ui.close();
                        }
                        let label = if cfg!(target_os = "windows") {
                            format!("Apply ({})", slot.shortcut())
                        } else {
                            "Apply".into()
                        };
                        if ui
                            .add_enabled(
                                self.controller.can_apply_profile(slot),
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            self.controller.apply_profile(slot);
                            ui.close();
                        }
                    });
                    ui.separator();
                }
                ui.weak("Saves each connected display separately.");
            });

            #[cfg(any(target_os = "linux", target_os = "windows"))]
            self.render_settings_menu(ui);

            let hide_label = if self.controller.tray_available() {
                "Hide to Tray"
            } else {
                "Quit"
            };
            if ui.button(hide_label).clicked() {
                if self.controller.tray_available() {
                    self.hide_to_tray(ui.ctx());
                } else {
                    self.quit(ui.ctx());
                }
            }

            if ui.button("Quit").clicked() {
                self.quit(ui.ctx());
            }
        });
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn render_settings_menu(&mut self, ui: &mut egui::Ui) {
        let config = egui::menu::MenuConfig::new()
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        let (_, menu) = egui::menu::MenuButton::new("Settings")
            .config(config)
            .ui(ui, |ui| {
                let setting = self.startup_setting.get_or_insert_with(|| {
                    match crate::startup::is_enabled() {
                        Ok(enabled) => StartupSetting {
                            enabled: Some(enabled),
                            error: None,
                        },
                        Err(error) => StartupSetting {
                            enabled: None,
                            error: Some(format!("Could not read startup setting: {error}")),
                        },
                    }
                });

                let mut checked = setting.enabled.unwrap_or_default();
                if ui
                    .add_enabled(
                        setting.enabled.is_some(),
                        egui::Checkbox::new(&mut checked, "Start at sign-in"),
                    )
                    .changed()
                {
                    match crate::startup::set_enabled(checked) {
                        Ok(()) => {
                            setting.enabled = Some(checked);
                            setting.error = None;
                        }
                        Err(error) => {
                            setting.error =
                                Some(format!("Could not change startup setting: {error}"));
                        }
                    }
                }

                if let Some(error) = &setting.error {
                    ui.small(RichText::new(error).color(ui.visuals().error_fg_color));
                }
            });
        if menu.is_none() {
            self.startup_setting = None;
        }
    }

    fn render_display_row(&mut self, ui: &mut egui::Ui, display: DisplayState) {
        let display_id = display.info.id.clone();
        let name = display.info.name;
        let kind = display.info.kind.label();
        let enabled = display.info.supported;
        let mut value = display.info.current.unwrap_or(0).min(100);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(DISPLAY_NAME_WIDTH);
                ui.add_sized(
                    [DISPLAY_NAME_WIDTH, 18.0],
                    egui::Label::new(name).truncate(),
                );
                ui.add_sized(
                    [DISPLAY_NAME_WIDTH, 16.0],
                    egui::Label::new(
                        RichText::new(kind)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    )
                    .truncate(),
                );
            });

            let percent = display
                .info
                .current
                .map(|current| format!("{}%", current.min(100)))
                .unwrap_or_else(|| "-".to_string());
            ui.add_sized([PERCENT_WIDTH, 20.0], egui::Label::new(percent));

            ui.scope(|ui| {
                self.render_brightness_slider(ui, &display_id, &mut value, enabled);
            });

            if step_button(ui, enabled, "−").clicked() {
                self.controller.adjust_brightness(display_id.clone(), -1);
                ui.ctx().request_repaint();
            }
            if step_button(ui, enabled, "+").clicked() {
                self.controller.adjust_brightness(display_id, 1);
                ui.ctx().request_repaint();
            }

            ui.allocate_ui_with_layout(
                [18.0, 20.0].into(),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if display.pending {
                        ui.spinner();
                    }
                },
            );
        });

        if let Some(error) = display.error {
            ui.small(RichText::new(error).color(ui.visuals().error_fg_color));
        }
    }

    fn render_brightness_slider(
        &self,
        ui: &mut egui::Ui,
        display_id: &DisplayId,
        value: &mut u32,
        enabled: bool,
    ) -> egui::Response {
        ui.spacing_mut().slider_width = SLIDER_WIDTH;
        let response = ui.add_enabled(enabled, Slider::new(value, 0..=100).show_value(false));
        if response.changed() {
            self.controller.set_brightness(display_id.clone(), *value);
            ui.ctx().request_repaint();
        }
        response
    }
}

fn step_button(ui: &mut egui::Ui, enabled: bool, label: &'static str) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        ui.add_sized(
            [STEP_BUTTON_SIZE, STEP_BUTTON_SIZE],
            egui::Button::new(label),
        )
    })
    .inner
}

impl eframe::App for MainWindow {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_close_request(ctx);
        self.handle_app_commands(ctx);

        if self.controller.drain_state_events() {
            ctx.request_repaint();
        }

        self.schedule_repaint(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.spacing_mut().item_spacing = [8.0, 4.0].into();

            self.render_toolbar(ui);
            ui.separator();

            let snapshot = self.controller.snapshot();
            if let Some(message) = &snapshot.profile_message {
                ui.small(message);
            }
            for error in [&snapshot.profile_error, &snapshot.hotkey_error]
                .into_iter()
                .flatten()
            {
                ui.small(RichText::new(error).color(ui.visuals().error_fg_color));
            }
            if let Some(error) = snapshot.last_error {
                ui.small(RichText::new(error).color(ui.visuals().error_fg_color));
                ui.separator();
            }

            if snapshot.displays.is_empty() {
                ui.label(
                    RichText::new("No displays detected").color(ui.visuals().weak_text_color()),
                );
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for display in snapshot.displays {
                        self.render_display_row(ui, display);
                        ui.separator();
                    }
                });
        });
    }
}

#[cfg(all(test, feature = "mock-backend"))]
mod tests {
    use super::*;
    use crate::{
        brightness::{mock::MockBrightnessBackend, BrightnessBackend},
        config::AppConfig,
    };
    use std::{sync::Arc, time::Instant};

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "brightness update timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn hide_releases_viewport_and_keeps_brightness_control_running() {
        let backend = Arc::new(MockBrightnessBackend::default());
        let controller = AppController::with_backend(AppConfig::default(), backend.clone());
        controller.set_tray_available(true);
        wait_for(|| !controller.snapshot().displays.is_empty());
        let mut window = MainWindow::new(controller.clone(), true);
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            window.hide_to_tray(ui.ctx());
        });
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands
            .iter()
            .any(|command| matches!(command, ViewportCommand::Close)));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, ViewportCommand::CancelClose)));
        assert!(!controller.is_quitting());

        let display_id = DisplayId::new("mock:internal");
        controller.set_brightness(display_id.clone(), 42);
        wait_for(|| backend.get_brightness(&display_id).unwrap().percent() == 42);
        controller.shutdown();
    }

    #[test]
    fn open_cancels_a_hide_requested_in_the_same_frame() {
        let controller = AppController::with_backend(
            AppConfig::default(),
            Arc::new(MockBrightnessBackend::default()),
        );
        controller.set_tray_available(true);
        let mut window = MainWindow::new(controller.clone(), true);
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            window.hide_to_tray(ui.ctx());
            controller.request_open_window();
            window.handle_app_commands(ui.ctx());
        });
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands
            .iter()
            .any(|command| matches!(command, ViewportCommand::CancelClose)));
        assert!(window.visible);
        assert!(!controller.is_quitting());
        controller.shutdown();
    }

    #[test]
    fn slider_changes_hardware_while_pointer_is_still_down() {
        let backend = Arc::new(MockBrightnessBackend::default());
        let controller = AppController::with_backend(AppConfig::default(), backend.clone());
        wait_for(|| !controller.snapshot().displays.is_empty());
        let window = MainWindow::new(controller.clone(), true);
        let context = egui::Context::default();
        let display_id = DisplayId::new("mock:internal");
        let mut value = 60;
        let mut slider_rect = egui::Rect::NOTHING;
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                slider_rect = window
                    .render_brightness_slider(ui, &display_id, &mut value, true)
                    .rect;
            });
        });
        let position = egui::pos2(
            slider_rect.left() + slider_rect.width() * 0.25,
            slider_rect.center().y,
        );
        let input = egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = context.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                window.render_brightness_slider(ui, &display_id, &mut value, true);
            });
        });

        assert!(context.input(|input| input.pointer.primary_down()));
        assert_ne!(value, 60);
        wait_for(|| backend.get_brightness(&display_id).unwrap().percent() == value);
        assert!(context.input(|input| input.pointer.primary_down()));

        let previous = value;
        let drag_position = egui::pos2(
            slider_rect.left() + slider_rect.width() * 0.75,
            slider_rect.center().y,
        );
        let _ = context.run_ui(
            egui::RawInput {
                events: vec![egui::Event::PointerMoved(drag_position)],
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    window.render_brightness_slider(ui, &display_id, &mut value, true);
                });
            },
        );
        assert!(value > previous);
        wait_for(|| backend.get_brightness(&display_id).unwrap().percent() == value);
        assert!(context.input(|input| input.pointer.primary_down()));
        controller.shutdown();
    }
}
