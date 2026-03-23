use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
pub enum Illuminance {
    /// Numeric lux value (e.g. IKEA VALLHORN)
    Lux(u16),
    /// Boolean threshold from sensor (true = bright enough, e.g. IKEA TRADFRI motion sensor 2)
    AboveThreshold(bool),
}

#[derive(Debug, Clone)]
pub enum Event {
    DevicesUpdated,
    GroupsUpdated,
    LightStateChanged {
        room_id: String,
    },
    MotionDetected {
        room_id: String,
        sensor_ieee: String,
        illuminance: Option<Illuminance>,
    },
    MotionCleared {
        room_id: String,
        sensor_ieee: String,
    },
    DeviceAvailabilityChanged {
        ieee: String,
        available: bool,
    },
    NightModeChanged {
        room_id: String,
        active: bool,
    },
    RemoteAction {
        remote_ieee: String,
        action: String,
    },
    ExternalLightChange {
        room_id: String,
        device_name: String,
    },
    ConfigReloaded,
    MqttConnected,
    MqttDisconnected,
}

#[derive(Clone)]
pub struct EventBus {
    sender: Arc<broadcast::Sender<Event>>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender: Arc::new(sender),
        }
    }

    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}
