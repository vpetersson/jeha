use std::sync::Arc;

use tokio::sync::broadcast;

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
