//! Protocol-independent wakeup for battery and split connection observers.
//!
//! Producers own the actual status. Consumers read their snapshots after a
//! wakeup; no BLE service type is exposed to the producers.

use embassy_sync::signal::Signal;

use crate::RawMutex;

static BATTERY_STATE_CHANGED: Signal<RawMutex, ()> = Signal::new();

pub(crate) fn notify_battery_state_changed() {
    BATTERY_STATE_CHANGED.signal(());
}

#[cfg(feature = "keyboard_system_status")]
pub(crate) async fn wait_battery_state_changed() {
    BATTERY_STATE_CHANGED.wait().await;
}
