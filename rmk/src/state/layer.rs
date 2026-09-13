use core::cell::Cell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;

use crate::RawMutex;

/// Maximum number of layers representable by [`LayerStateSnapshot`].
pub const MAX_LAYER_STATE_LAYERS: usize = u8::MAX as usize + 1;

/// Protocol-independent snapshot of the keymap's effective layer state.
///
/// The default layer is always included in `active_bitmap`, even though RMK
/// stores momentary/toggled activation separately from the default layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerStateSnapshot {
    default_layer: u8,
    layer_count: u16,
    active_bitmap: [u8; MAX_LAYER_STATE_LAYERS / 8],
}

impl LayerStateSnapshot {
    const EMPTY: Self = Self {
        default_layer: 0,
        layer_count: 0,
        active_bitmap: [0; MAX_LAYER_STATE_LAYERS / 8],
    };

    pub fn default_layer(self) -> u8 {
        self.default_layer
    }

    pub fn layer_count(self) -> usize {
        self.layer_count as usize
    }

    pub fn is_active(&self, layer: u8) -> bool {
        let layer = layer as usize;
        layer < self.layer_count() && self.active_bitmap[layer / 8] & (1 << (layer % 8)) != 0
    }

    pub fn active_bitmap(&self) -> &[u8; MAX_LAYER_STATE_LAYERS / 8] {
        &self.active_bitmap
    }

    pub(crate) fn from_keymap(default_layer: u8, layer_state: &[bool]) -> Self {
        assert!(
            !layer_state.is_empty() && layer_state.len() <= MAX_LAYER_STATE_LAYERS,
            "RMK layer state must contain between 1 and 256 layers"
        );
        assert!(
            usize::from(default_layer) < layer_state.len(),
            "default layer must be within the configured layer range"
        );

        let mut active_bitmap = [0; MAX_LAYER_STATE_LAYERS / 8];
        for (layer, active) in layer_state.iter().copied().enumerate() {
            if active || layer == usize::from(default_layer) {
                active_bitmap[layer / 8] |= 1 << (layer % 8);
            }
        }
        Self {
            default_layer,
            layer_count: layer_state.len() as u16,
            active_bitmap,
        }
    }
}

static LAYER_STATE: Mutex<RawMutex, Cell<LayerStateSnapshot>> =
    Mutex::new(Cell::new(LayerStateSnapshot::EMPTY));
static LAYER_STATE_CHANGED: Signal<RawMutex, ()> = Signal::new();

/// Return one consistent copy of the current default and active layers.
pub fn current_layer_state() -> LayerStateSnapshot {
    LAYER_STATE.lock(|state| state.get())
}

/// Replace the current keymap layer snapshot and wake internal consumers when
/// its effective value changed.
pub(crate) fn update_layer_state(default_layer: u8, layer_state: &[bool]) {
    let next = LayerStateSnapshot::from_keymap(default_layer, layer_state);

    let changed = LAYER_STATE.lock(|state| {
        if state.get() == next {
            false
        } else {
            state.set(next);
            true
        }
    });
    if changed {
        LAYER_STATE_CHANGED.signal(());
    }
}

pub(crate) async fn wait_layer_state_changed() {
    LAYER_STATE_CHANGED.wait().await;
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::{LAYER_STATE_CHANGED, LayerStateSnapshot, update_layer_state};

    fn layer_state_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn layer_snapshot_reports_default_and_all_active_layers() {
        let mut layers = [false; 256];
        layers[1] = true;
        layers[17] = true;
        layers[255] = true;

        let state = LayerStateSnapshot::from_keymap(3, &layers);

        assert_eq!(state.default_layer(), 3);
        assert_eq!(state.layer_count(), 256);
        assert!(state.is_active(1));
        assert!(state.is_active(3));
        assert!(state.is_active(17));
        assert!(state.is_active(255));
        assert!(!state.is_active(2));
    }

    #[test]
    fn unchanged_layer_snapshot_does_not_signal() {
        let _guard = layer_state_test_lock().lock().unwrap();
        while LAYER_STATE_CHANGED.try_take().is_some() {}

        update_layer_state(0, &[false, false]);
        LAYER_STATE_CHANGED.try_take();
        update_layer_state(0, &[false, false]);
        assert!(LAYER_STATE_CHANGED.try_take().is_none());

        update_layer_state(1, &[false, false]);
        assert!(LAYER_STATE_CHANGED.try_take().is_some());
    }
}
