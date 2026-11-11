use std::time::{Duration, Instant};

use kemolight::{app::AppController, brightness::create_backend, config::AppConfig};

fn main() {
    let config = AppConfig::load().expect("failed to load app config");
    let backend = create_backend(config.preferred_backend);

    // Read the real backend without changing any display's brightness.
    let started = Instant::now();
    let controller = AppController::with_backend(config, backend.clone());
    let deadline = started + Duration::from_secs(60);
    while !controller.drain_state_events() {
        assert!(Instant::now() < deadline, "initial discovery timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
    let elapsed = started.elapsed();
    let snapshot = controller.snapshot();
    controller.shutdown();
    assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
    println!(
        "worker_ready elapsed_ms={:.1} displays={}",
        elapsed.as_secs_f64() * 1000.0,
        snapshot.displays.len()
    );

    for pass in 1..=3 {
        let started = Instant::now();
        let displays = backend.list_displays().expect("display discovery failed");
        println!(
            "discovery pass={pass} elapsed_ms={:.1}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        for display in displays {
            println!(
                "  id={} supported={} current={:?}",
                display.id, display.supported, display.current
            );
        }
    }
}
