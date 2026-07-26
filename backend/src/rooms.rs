//! In-memory room state: signaling channels for a host and its viewers.
//! No media (RTP) is relayed here, only offer/answer text messages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

/// State for a single room: one host, zero or more viewers.
pub struct Room {
    host_tx: UnboundedSender<String>,
    viewers: Mutex<HashMap<String, UnboundedSender<String>>>,
}

impl Room {
    /// Sends a message to the host. Silently ignored if the host socket
    /// already closed.
    pub fn notify_host(&self, msg: String) {
        let _ = self.host_tx.send(msg);
    }

    pub fn add_viewer(&self, viewer_id: String, tx: UnboundedSender<String>) {
        self.viewers.lock().unwrap().insert(viewer_id, tx);
    }

    pub fn remove_viewer(&self, viewer_id: &str) {
        self.viewers.lock().unwrap().remove(viewer_id);
    }

    pub fn send_to_viewer(&self, viewer_id: &str, msg: String) {
        if let Some(tx) = self.viewers.lock().unwrap().get(viewer_id) {
            let _ = tx.send(msg);
        }
    }
}

#[derive(Default)]
pub struct RoomStore {
    rooms: Mutex<HashMap<String, Arc<Room>>>,
}

impl RoomStore {
    /// Registers a host's WebSocket connection as a room. Reuses
    /// `requested_code` if given and currently free (so a reconnecting
    /// host can keep the same room code); otherwise generates a new one.
    pub fn register_host(
        &self,
        requested_code: Option<&str>,
        host_tx: UnboundedSender<String>,
    ) -> String {
        let mut rooms = self.rooms.lock().unwrap();
        let code = match requested_code {
            Some(code) if !rooms.contains_key(code) => code.to_owned(),
            _ => {
                let mut candidate = crate::roomcode::generate();
                while rooms.contains_key(&candidate) {
                    candidate = crate::roomcode::generate();
                }
                candidate
            }
        };
        rooms.insert(
            code.clone(),
            Arc::new(Room {
                host_tx,
                viewers: Mutex::new(HashMap::new()),
            }),
        );
        code
    }

    pub fn get(&self, room_code: &str) -> Option<Arc<Room>> {
        self.rooms.lock().unwrap().get(room_code).cloned()
    }

    /// Removes a room when its host disconnects.
    pub fn remove_room(&self, room_code: &str) {
        self.rooms.lock().unwrap().remove(room_code);
    }
}
