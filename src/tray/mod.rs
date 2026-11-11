use std::sync::mpsc::{SyncSender, TrySendError};

use crate::app::AppController;

#[cfg(all(target_os = "linux", feature = "tray"))]
pub mod linux;
pub mod mock;
#[cfg(all(target_os = "windows", feature = "tray"))]
pub mod windows;

pub struct TrayHandle {
    available: bool,
    update_tx: Option<SyncSender<()>>,
    shutdown: Option<Box<dyn FnOnce()>>,
}

impl TrayHandle {
    pub fn unavailable() -> Self {
        Self {
            available: false,
            update_tx: None,
            shutdown: None,
        }
    }

    pub fn available(update_tx: Option<SyncSender<()>>, shutdown: impl FnOnce() + 'static) -> Self {
        Self {
            available: true,
            update_tx,
            shutdown: Some(Box::new(shutdown)),
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn update_sender(&self) -> Option<SyncSender<()>> {
        self.update_tx.clone()
    }

    pub fn request_update(&self) -> bool {
        self.update_tx.as_ref().is_some_and(|update_tx| {
            matches!(update_tx.try_send(()), Ok(()) | Err(TrySendError::Full(())))
        })
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown();
        }
    }
}

pub fn start_tray(controller: AppController) -> TrayHandle {
    #[cfg(all(target_os = "linux", feature = "tray"))]
    {
        linux::start_tray(controller)
    }

    #[cfg(all(target_os = "windows", feature = "tray"))]
    {
        windows::start_tray(controller)
    }

    #[cfg(not(any(
        all(target_os = "linux", feature = "tray"),
        all(target_os = "windows", feature = "tray")
    )))]
    {
        mock::start_tray(controller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn unavailable_tray_update_is_noop() {
        let handle = TrayHandle::unavailable();

        assert!(!handle.is_available());
        assert!(handle.update_sender().is_none());
        assert!(!handle.request_update());
    }

    #[test]
    fn available_tray_update_sends_event() {
        let (tx, rx) = mpsc::sync_channel(1);
        let handle = TrayHandle::available(Some(tx), || {});

        assert!(handle.is_available());
        assert!(handle.update_sender().is_some());
        assert!(handle.request_update());
        for _ in 0..10_000 {
            assert!(handle.request_update());
        }
        assert_eq!(rx.try_iter().count(), 1);
        assert!(handle.request_update());
        assert!(rx.try_recv().is_ok());
        drop(rx);
        assert!(!handle.request_update());
    }
}
