#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "windows")]
    if kemolight::ui_process::run_child_if_requested() {
        return;
    }

    let Some(_single_instance) = kemolight::single_instance::SingleInstance::acquire() else {
        return;
    };

    kemolight::app::run();
}
