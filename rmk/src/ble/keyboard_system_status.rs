//! Optional vendor-specific GATT service for keyboard system status.
//!
//! The transport and wire format are keyboard-independent. Applications provide
//! an ordered list of nodes and select where each node's battery value comes
//! from.

use core::cell::RefCell;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use heapless::Vec;
use rmk_types::battery::BatteryStatus;
use trouble_host::prelude::*;

use super::ble_server::Server;
use crate::core_traits::Runnable;

pub const MAX_NODES: usize = 8;
pub const MAX_SYSTEM_INFO_LEN: usize = 3 + MAX_NODES;
pub const UNKNOWN_BATTERY_LEVEL: u8 = 0xff;
pub const KEYBOARD_SYSTEM_STATUS_SERVICE_UUID: u128 = 0x291c4b33_35fd_4c51_913e_109541021218;
pub const SYSTEM_INFO_UUID: u128 = 0xb7fb6869_d828_4f31_9ad8_949da7df35dc;
pub const NODE_STATUS_UUID: u128 = 0xe5c2c1e1_025b_4b09_be4e_4d052525e4d0;
pub const LAYER_STATUS_UUID: u128 = 0x68777b88_895f_4d13_b433_adb93a243c6b;

const VERSION_MAJOR: u8 = 1;
const VERSION_MINOR: u8 = 0;

/// A node's role in the keyboard system status protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeRole {
    Main = 0x00,
    Left = 0x01,
    Right = 0x02,
    PointingDevice = 0x03,
    Numpad = 0x04,
    Other = 0xff,
}

/// Source used to populate a node's battery status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatterySource {
    /// Battery measured by the keyboard's BLE host node.
    Central,
    /// Battery reported by split peripheral `id`.
    Peripheral(usize),
    /// The node has no battery or its battery is intentionally not exposed.
    None,
}

/// Application-provided description of one status node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    pub role: NodeRole,
    pub battery_source: BatterySource,
}

impl NodeConfig {
    pub const fn new(role: NodeRole, battery_source: BatterySource) -> Self {
        Self {
            role,
            battery_source,
        }
    }
}

/// Invalid keyboard system status configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    NoNodes,
    TooManyNodes,
    SplitFeatureRequired,
    PeripheralOutOfRange(usize),
}

/// Ordered node configuration shared by System Info and Node Status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardSystemStatusConfig {
    nodes: Vec<NodeConfig, MAX_NODES>,
}

impl KeyboardSystemStatusConfig {
    pub fn new(nodes: &[NodeConfig]) -> Result<Self, ConfigError> {
        if nodes.is_empty() {
            return Err(ConfigError::NoNodes);
        }
        if nodes.len() > MAX_NODES {
            return Err(ConfigError::TooManyNodes);
        }

        for node in nodes {
            if let BatterySource::Peripheral(id) = node.battery_source {
                #[cfg(not(feature = "split"))]
                {
                    let _ = id;
                    return Err(ConfigError::SplitFeatureRequired);
                }
                #[cfg(feature = "split")]
                if id >= crate::SPLIT_PERIPHERALS_NUM {
                    return Err(ConfigError::PeripheralOutOfRange(id));
                }
            }
        }

        let nodes = Vec::from_slice(nodes).map_err(|_| ConfigError::TooManyNodes)?;
        Ok(Self { nodes })
    }

    pub fn nodes(&self) -> &[NodeConfig] {
        &self.nodes
    }
}

static CONFIG: Mutex<crate::RawMutex, RefCell<Option<KeyboardSystemStatusConfig>>> =
    Mutex::new(RefCell::new(None));
static STATUS_CHANGED: Signal<crate::RawMutex, ()> = Signal::new();

pub(crate) fn notify_status_changed() {
    STATUS_CHANGED.signal(());
}

fn install_config(config: KeyboardSystemStatusConfig) {
    CONFIG.lock(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(
            slot.is_none(),
            "Keyboard System Status config installed more than once"
        );
        *slot = Some(config);
    });
    STATUS_CHANGED.signal(());
}

fn with_config<T>(f: impl FnOnce(&KeyboardSystemStatusConfig) -> T) -> Option<T> {
    CONFIG.lock(|slot| slot.borrow().as_ref().map(f))
}

fn system_info() -> Vec<u8, MAX_SYSTEM_INFO_LEN> {
    with_config(|config| encode_system_info(config.nodes())).unwrap_or_default()
}

fn encode_system_info(nodes: &[NodeConfig]) -> Vec<u8, MAX_SYSTEM_INFO_LEN> {
    let mut value = Vec::new();
    value.push(VERSION_MAJOR).expect("System Info capacity");
    value.push(VERSION_MINOR).expect("System Info capacity");
    value
        .push(u8::try_from(nodes.len()).expect("node count validated"))
        .expect("System Info capacity");
    for node in nodes {
        value.push(node.role as u8).expect("System Info capacity");
    }
    value
}

fn battery_level(status: BatteryStatus) -> u8 {
    match status {
        BatteryStatus::Available {
            level: Some(level), ..
        } if level <= 100 => level,
        _ => UNKNOWN_BATTERY_LEVEL,
    }
}

fn node_status() -> Vec<u8, MAX_NODES> {
    with_config(|config| {
        let mut value = Vec::new();
        for node in config.nodes() {
            let level = match node.battery_source {
                BatterySource::Central => {
                    battery_level(crate::input_device::battery::current_battery_status())
                }
                #[cfg(feature = "split")]
                BatterySource::Peripheral(id) => {
                    crate::split::driver::current_connected_peripheral_battery_status(id)
                        .map(battery_level)
                        .unwrap_or(UNKNOWN_BATTERY_LEVEL)
                }
                #[cfg(not(feature = "split"))]
                BatterySource::Peripheral(_) => UNKNOWN_BATTERY_LEVEL,
                BatterySource::None => UNKNOWN_BATTERY_LEVEL,
            };
            value.push(level).expect("node count validated");
        }
        value
    })
    .unwrap_or_default()
}

fn layer_status() -> [u8; 5] {
    encode_layer_status(crate::state::current_layer_state())
}

fn encode_layer_status(state: crate::state::LayerStateSnapshot) -> [u8; 5] {
    assert!(
        state.layer_count() <= 32,
        "Keyboard System Status v1 supports at most 32 layers"
    );
    assert!(
        state.default_layer() < 32,
        "Default layer must fit the v1 layer bitmap"
    );
    let bitmap = state.active_bitmap();
    [
        state.default_layer(),
        bitmap[0],
        bitmap[1],
        bitmap[2],
        bitmap[3],
    ]
}

/// Startup processor that installs the application-provided node config.
///
/// Battery and split drivers signal changes directly, so this task consumes no
/// event subscriber slots after initialization.
pub struct KeyboardSystemStatusProcessor;

impl KeyboardSystemStatusProcessor {
    pub fn new(config: KeyboardSystemStatusConfig) -> Self {
        install_config(config);
        Self
    }

    pub async fn process_loop(&mut self) -> ! {
        core::future::pending().await
    }
}

impl Runnable for KeyboardSystemStatusProcessor {
    async fn run(&mut self) -> ! {
        self.process_loop().await
    }
}

/// Keyboard System Status Service v1.
#[gatt_service(uuid = KEYBOARD_SYSTEM_STATUS_SERVICE_UUID)]
pub(crate) struct KeyboardSystemStatusService {
    #[characteristic(
        uuid = SYSTEM_INFO_UUID,
        read,
        permissions(encrypted)
    )]
    pub(crate) system_info: Vec<u8, MAX_SYSTEM_INFO_LEN>,
    #[characteristic(
        uuid = NODE_STATUS_UUID,
        read,
        notify,
        permissions(encrypted)
    )]
    pub(crate) node_status: Vec<u8, MAX_NODES>,
    #[characteristic(
        uuid = LAYER_STATUS_UUID,
        read,
        notify,
        permissions(encrypted)
    )]
    pub(crate) layer_status: [u8; 5],
}

pub(crate) fn initialize_server(server: &Server) {
    let system_info = system_info();
    if system_info.is_empty() {
        error!("Keyboard System Status enabled without installing a node config");
        return;
    }

    if let Err(e) = server.set(
        &server.keyboard_system_status_service.system_info,
        &system_info,
    ) {
        error!("Failed to initialize System Info: {:?}", e);
    }
    if let Err(e) = server.set(
        &server.keyboard_system_status_service.node_status,
        &node_status(),
    ) {
        error!("Failed to initialize Node Status: {:?}", e);
    }
    if let Err(e) = server.set(
        &server.keyboard_system_status_service.layer_status,
        &layer_status(),
    ) {
        error!("Failed to initialize Layer Status: {:?}", e);
    }
}

pub(crate) fn is_cccd_handle(server: &Server, handle: u16) -> bool {
    handle
        == server
            .keyboard_system_status_service
            .node_status
            .cccd_handle
            .expect("No CCCD for Node Status")
        || handle
            == server
                .keyboard_system_status_service
                .layer_status
                .cccd_handle
                .expect("No CCCD for Layer Status")
}

pub(crate) struct BleKeyboardSystemStatusServer<'stack, 'server, 'conn, P: PacketPool> {
    node_status: Characteristic<Vec<u8, MAX_NODES>>,
    layer_status: Characteristic<[u8; 5]>,
    conn: &'conn GattConnection<'stack, 'server, P>,
    last_node_status: Vec<u8, MAX_NODES>,
    last_layer_status: [u8; 5],
}

impl<'stack, 'server, 'conn, P: PacketPool>
    BleKeyboardSystemStatusServer<'stack, 'server, 'conn, P>
{
    pub(crate) fn new(server: &Server, conn: &'conn GattConnection<'stack, 'server, P>) -> Self {
        let last_node_status = node_status();
        let last_layer_status = layer_status();

        if let Err(e) = server.set(
            &server.keyboard_system_status_service.node_status,
            &last_node_status,
        ) {
            warn!("Failed to refresh Node Status: {:?}", e);
        }
        if let Err(e) = server.set(
            &server.keyboard_system_status_service.layer_status,
            &last_layer_status,
        ) {
            warn!("Failed to refresh Layer Status: {:?}", e);
        }

        Self {
            node_status: server.keyboard_system_status_service.node_status.clone(),
            layer_status: server.keyboard_system_status_service.layer_status,
            conn,
            last_node_status,
            last_layer_status,
        }
    }
}

impl<P: PacketPool> Runnable for BleKeyboardSystemStatusServer<'_, '_, '_, P> {
    async fn run(&mut self) -> ! {
        loop {
            select(
                STATUS_CHANGED.wait(),
                crate::state::wait_layer_state_changed(),
            )
            .await;

            let next_node_status = node_status();
            if next_node_status != self.last_node_status {
                if let Err(e) = self
                    .node_status
                    .notify(self.conn, &next_node_status, true)
                    .await
                {
                    warn!("Failed to update Node Status: {:?}", e);
                }
                self.last_node_status = next_node_status;
            }

            let next_layer_status = layer_status();
            if next_layer_status != self.last_layer_status {
                if let Err(e) = self
                    .layer_status
                    .notify(self.conn, &next_layer_status, true)
                    .await
                {
                    warn!("Failed to update Layer Status: {:?}", e);
                }
                self.last_layer_status = next_layer_status;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_any_valid_node_order() {
        let nodes = [
            NodeConfig::new(NodeRole::Numpad, BatterySource::None),
            NodeConfig::new(NodeRole::Main, BatterySource::Central),
            NodeConfig::new(NodeRole::PointingDevice, BatterySource::None),
        ];

        assert_eq!(encode_system_info(&nodes).as_slice(), &[1, 0, 3, 4, 0, 3]);
    }

    #[test]
    fn validates_node_count() {
        assert_eq!(
            KeyboardSystemStatusConfig::new(&[]),
            Err(ConfigError::NoNodes)
        );
        let nodes = [NodeConfig::new(NodeRole::Other, BatterySource::None); MAX_NODES + 1];
        assert_eq!(
            KeyboardSystemStatusConfig::new(&nodes),
            Err(ConfigError::TooManyNodes)
        );
    }

    #[cfg(feature = "split")]
    #[test]
    fn validates_peripheral_source_range() {
        if crate::SPLIT_PERIPHERALS_NUM > 0 {
            let valid = [NodeConfig::new(
                NodeRole::Other,
                BatterySource::Peripheral(0),
            )];
            assert!(KeyboardSystemStatusConfig::new(&valid).is_ok());
        }

        let invalid = [NodeConfig::new(
            NodeRole::Other,
            BatterySource::Peripheral(crate::SPLIT_PERIPHERALS_NUM),
        )];
        assert_eq!(
            KeyboardSystemStatusConfig::new(&invalid),
            Err(ConfigError::PeripheralOutOfRange(
                crate::SPLIT_PERIPHERALS_NUM
            ))
        );
    }

    #[test]
    fn layer_wire_format_is_little_endian_and_includes_default() {
        let mut layers = [false; 32];
        layers[1] = true;
        layers[17] = true;

        assert_eq!(
            encode_layer_status(crate::state::LayerStateSnapshot::from_keymap(3, &layers)),
            [3, 0x0a, 0x00, 0x02, 0x00]
        );
    }

    #[test]
    #[should_panic(expected = "supports at most 32 layers")]
    fn layer_wire_format_rejects_more_than_v1_can_represent() {
        encode_layer_status(crate::state::LayerStateSnapshot::from_keymap(
            0,
            &[false; 33],
        ));
    }

    #[test]
    fn unavailable_and_reserved_battery_levels_are_unknown() {
        use rmk_types::battery::ChargeState;

        assert_eq!(
            battery_level(BatteryStatus::Unavailable),
            UNKNOWN_BATTERY_LEVEL
        );
        assert_eq!(
            battery_level(BatteryStatus::Available {
                charge_state: ChargeState::Discharging,
                level: Some(101),
            }),
            UNKNOWN_BATTERY_LEVEL
        );
    }

    #[test]
    fn gatt_characteristics_have_v1_properties_and_permissions() {
        use embassy_sync::blocking_mutex::raw::NoopRawMutex;
        use trouble_host::attribute::{AttributeTable, PermissionLevel};

        let mut table: AttributeTable<NoopRawMutex, 9> = AttributeTable::new();
        let service = KeyboardSystemStatusService::new(&mut table);

        assert_eq!(KeyboardSystemStatusService::ATTRIBUTE_COUNT, 9);
        assert_eq!(KeyboardSystemStatusService::CCCD_COUNT, 2);
        let mut service_uuid = [0; 16];
        assert_eq!(
            table.read(service.handle, 0, &mut service_uuid).unwrap(),
            16
        );
        assert_eq!(
            service_uuid,
            KEYBOARD_SYSTEM_STATUS_SERVICE_UUID.to_le_bytes()
        );
        assert_eq!(service.system_info.uuid, SYSTEM_INFO_UUID.into());
        assert_eq!(service.node_status.uuid, NODE_STATUS_UUID.into());
        assert_eq!(service.layer_status.uuid, LAYER_STATUS_UUID.into());
        assert!(service.system_info.props.any(&[CharacteristicProp::Read]));
        assert!(!service.system_info.props.any(&[CharacteristicProp::Notify]));
        assert!(service.node_status.props.any(&[CharacteristicProp::Read]));
        assert!(service.node_status.props.any(&[CharacteristicProp::Notify]));
        assert!(service.layer_status.props.any(&[CharacteristicProp::Read]));
        assert!(
            service
                .layer_status
                .props
                .any(&[CharacteristicProp::Notify])
        );

        for handle in [
            service.system_info.handle,
            service.node_status.handle,
            service.layer_status.handle,
        ] {
            let permissions = table
                .permissions(handle)
                .expect("characteristic value handle");
            assert_eq!(permissions.read, PermissionLevel::EncryptionRequired);
            assert_eq!(permissions.write, PermissionLevel::NotAllowed);
        }

        for handle in [
            service.node_status.cccd_handle.expect("Node Status CCCD"),
            service.layer_status.cccd_handle.expect("Layer Status CCCD"),
        ] {
            let permissions = table.permissions(handle).expect("CCCD handle");
            assert_eq!(permissions.write, PermissionLevel::EncryptionRequired);
        }
    }
}
