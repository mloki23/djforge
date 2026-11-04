//! Port identity and the open-port table shared by [`crate::MidiInputs`] and [`crate::FakeSource`].
//!
//! A port has three names:
//! - its **backend id** (midir's `MidiInputPort::id()`). On WinMM it is the device-interface path:
//!   stable for the same device in the same socket, distinct for two identical devices, but
//!   *shared by every port of one multi-port device* (a Launchpad's MIDI and DAW ports). On
//!   CoreMIDI it is the endpoint's unique id, stable across replugs. On ALSA it is `client:port`,
//!   which the system reassigns on replug;
//! - its **raw name**, as the backend reports it;
//! - its **listed name**, the raw name made unique within one listing by a " #2", " #3" suffix.
//!   The suffix only disambiguates within that listing: when the first of two identical controllers
//!   is unplugged, the second one's listed name changes from "X #2" to "X". Persist mappings on
//!   raw name plus backend id, not on the suffixed name.
//!
//! A port's identity is the pair (backend id, raw name), the same pair midir matches ports on, so
//! the ports of one device stay distinct. [`PortId`]s are keyed on it: a port keeps its id across
//! renames, and across replugs on Windows and macOS. On ALSA a replugged device gets a new
//! `client:port`, so it reads as `Closed` then `Appeared` and gets a new id.

use std::collections::HashSet;

use crate::{MidiIoError, PortId, PortInfo};

/// A change in the device list, reported by [`crate::MidiSource::refresh`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortChange {
    /// A device appeared (not opened automatically).
    Appeared { name: String, raw_name: String, backend_id: String },
    /// A device that was not open went away.
    Disappeared { name: String, raw_name: String, backend_id: String },
    /// An open port's device went away or its connection died; the port has been closed. Release
    /// any notes still held from it.
    Closed { port: PortId, name: String },
    /// An open port's listed name changed (a duplicate ahead of it went away). Same device, same id.
    Renamed { port: PortId, from: String, to: String },
}

impl PortInfo {
    /// Whether two listings describe the same port: same backend id and same raw name.
    pub fn same_port(&self, other: &PortInfo) -> bool {
        self.backend_id == other.backend_id && self.raw_name == other.raw_name
    }
}

/// One known port: its identity and latest listed name.
#[derive(Debug)]
struct IdEntry {
    backend_id: String,
    raw_name: String,
    name: String,
}

/// Port id by identity (backend id, raw name), with each port's latest listed name. Stable for the
/// life of a source.
#[derive(Debug, Default)]
pub(crate) struct PortIds {
    /// The index is the id.
    entries: Vec<IdEntry>,
}

impl PortIds {
    /// The id for a port, created on first sight; records its listed name as current.
    pub(crate) fn id_for(&mut self, info: &PortInfo) -> PortId {
        if let Some(i) = self.entries.iter().position(|e| e.backend_id == info.backend_id && e.raw_name == info.raw_name) {
            if let Some(e) = self.entries.get_mut(i) {
                info.name.clone_into(&mut e.name);
            }
            return PortId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        self.entries.push(IdEntry { backend_id: info.backend_id.clone(), raw_name: info.raw_name.clone(), name: info.name.clone() });
        PortId(u32::try_from(self.entries.len().saturating_sub(1)).unwrap_or(u32::MAX))
    }

    fn entry(&self, id: PortId) -> Option<&IdEntry> {
        self.entries.get(usize::try_from(id.0).ok()?)
    }

    pub(crate) fn name(&self, id: PortId) -> Option<&str> {
        self.entry(id).map(|e| e.name.as_str())
    }

    pub(crate) fn raw_name(&self, id: PortId) -> Option<&str> {
        self.entry(id).map(|e| e.raw_name.as_str())
    }

    pub(crate) fn backend_id(&self, id: PortId) -> Option<&str> {
        self.entry(id).map(|e| e.backend_id.as_str())
    }
}

/// Makes device names unique in list order: the second "X" becomes "X #2", the third "X #3".
pub(crate) fn unique_names(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for name in raw {
        let mut candidate = name.clone();
        let mut n = 2u32;
        while seen.contains(&candidate) {
            candidate = format!("{name} #{n}");
            n = n.saturating_add(1);
        }
        seen.insert(candidate.clone());
        out.push(candidate);
    }
    out
}

/// A listing from `(raw name, backend id)` pairs in device order.
pub(crate) fn listing(devices: impl IntoIterator<Item = (String, String)>) -> Vec<PortInfo> {
    let (raw, ids): (Vec<String>, Vec<String>) = devices.into_iter().unzip();
    unique_names(raw.iter().cloned())
        .into_iter()
        .zip(raw)
        .zip(ids)
        .enumerate()
        .map(|(index, ((name, raw_name), backend_id))| PortInfo { name, raw_name, index, backend_id })
        .collect()
}

/// One open connection.
pub(crate) struct OpenPort<C> {
    pub(crate) id: PortId,
    /// The listing it was opened from, with `name` kept current.
    pub(crate) info: PortInfo,
    pub(crate) conn: C,
}

/// The open ports of a source plus what it last saw listed. `C` is the connection; dropping it
/// closes the port.
pub(crate) struct PortTable<C> {
    ids: PortIds,
    open: Vec<OpenPort<C>>,
    /// Backend ids and names in the previous listing; `None` before the first.
    known: Option<Vec<PortInfo>>,
    /// Changes found while opening, returned by the next `refresh`.
    pending: Vec<PortChange>,
}

impl<C> Default for PortTable<C> {
    fn default() -> Self {
        Self { ids: PortIds::default(), open: Vec::new(), known: None, pending: Vec::new() }
    }
}

impl<C> PortTable<C> {
    /// Brings the table in line with `now`: closes open ports whose device is gone or whose
    /// connection `alive` says is dead, follows renames, and notes appearances. Changes are queued.
    fn reconcile(&mut self, now: &[PortInfo], alive: impl Fn(&C) -> bool) {
        let mut kept = Vec::with_capacity(self.open.len());
        let mut closed: Vec<PortInfo> = Vec::new();
        for mut p in self.open.drain(..) {
            match now.iter().find(|q| q.same_port(&p.info)) {
                Some(info) if alive(&p.conn) => {
                    if info.name != p.info.name {
                        let from = std::mem::replace(&mut p.info.name, info.name.clone());
                        self.pending.push(PortChange::Renamed { port: p.id, from, to: info.name.clone() });
                    }
                    kept.push(p);
                }
                // Dropping `p` closes the dead connection.
                _ => {
                    self.pending.push(PortChange::Closed { port: p.id, name: p.info.name.clone() });
                    closed.push(p.info);
                }
            }
        }
        self.open = kept;
        for p in &self.open {
            self.ids.id_for(&p.info);
        }
        let before = self.known.take().unwrap_or_default();
        for p in now.iter().filter(|p| !before.iter().any(|q| q.same_port(p))) {
            self.pending.push(PortChange::Appeared { name: p.name.clone(), raw_name: p.raw_name.clone(), backend_id: p.backend_id.clone() });
        }
        // Open ones that vanished were reported as `Closed` above.
        let gone = |q: &&PortInfo| {
            !now.iter().any(|p| p.same_port(q)) && !self.open.iter().any(|p| p.info.same_port(q)) && !closed.iter().any(|c| c.same_port(q))
        };
        for q in before.iter().filter(gone) {
            self.pending.push(PortChange::Disappeared { name: q.name.clone(), raw_name: q.raw_name.clone(), backend_id: q.backend_id.clone() });
        }
        self.known = Some(now.to_vec());
    }

    /// Reconciles with `now` and returns every change since the last call.
    pub(crate) fn refresh(&mut self, now: &[PortInfo], alive: impl Fn(&C) -> bool) -> Vec<PortChange> {
        self.reconcile(now, alive);
        std::mem::take(&mut self.pending)
    }

    /// Opens the port listed as `name` in `now`. If that port is already open (under any name) and
    /// alive, returns its id without connecting again; a dead connection is replaced.
    pub(crate) fn open(
        &mut self,
        now: &[PortInfo],
        name: &str,
        alive: impl Fn(&C) -> bool,
        connect: impl FnOnce(PortId, &PortInfo) -> Result<C, MidiIoError>,
    ) -> Result<PortId, MidiIoError> {
        self.reconcile(now, alive);
        let info = now.iter().find(|p| p.name == name).ok_or_else(|| MidiIoError::NoSuchPort(name.to_string()))?;
        if let Some(p) = self.open.iter().find(|p| p.info.same_port(info)) {
            return Ok(p.id);
        }
        let id = self.ids.id_for(info);
        let conn = connect(id, info)?;
        self.open.push(OpenPort { id, info: info.clone(), conn });
        Ok(id)
    }

    /// Removes the port open as `name`, returning its connection to close.
    pub(crate) fn close(&mut self, name: &str) -> Option<C> {
        let i = self.open.iter().position(|p| p.info.name == name)?;
        Some(self.open.swap_remove(i).conn)
    }

    /// Closes everything (connections drop in order).
    pub(crate) fn close_all(&mut self) {
        self.open.clear();
    }

    pub(crate) fn is_open(&self, name: &str) -> bool {
        self.open.iter().any(|p| p.info.name == name)
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut OpenPort<C>> {
        self.open.iter_mut().find(|p| p.info.name == name)
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut OpenPort<C>> {
        self.open.iter_mut()
    }

    pub(crate) fn port_name(&self, id: PortId) -> Option<&str> {
        self.ids.name(id)
    }

    pub(crate) fn raw_name(&self, id: PortId) -> Option<&str> {
        self.ids.raw_name(id)
    }

    pub(crate) fn backend_id(&self, id: PortId) -> Option<&str> {
        self.ids.backend_id(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_get_a_suffix() {
        let names = unique_names(["Pads", "Keys", "Pads", "Pads #2", "Pads"].map(String::from));
        assert_eq!(names, vec!["Pads", "Keys", "Pads #2", "Pads #2 #2", "Pads #3"]);
    }

    #[test]
    fn port_ids_follow_backend_id_and_raw_name() {
        let mut ids = PortIds::default();
        let a = ids.id_for(&info("X", "X", "usb-a"));
        let b = ids.id_for(&info("X #2", "X", "usb-b"));
        assert_ne!(a, b);
        // Same port, renamed: same id, new name.
        assert_eq!(ids.id_for(&info("X", "X", "usb-b")), b);
        assert_eq!(ids.name(b), Some("X"));
        assert_eq!(ids.backend_id(b), Some("usb-b"));
        // Another port of the same device (shared backend id): its own id.
        let daw = ids.id_for(&info("X DAW", "X DAW", "usb-b"));
        assert_ne!(daw, b);
        assert_eq!(ids.raw_name(daw), Some("X DAW"));
        assert_eq!(ids.name(PortId(99)), None);
    }

    fn info(name: &str, raw_name: &str, backend_id: &str) -> PortInfo {
        PortInfo { name: name.into(), raw_name: raw_name.into(), index: 0, backend_id: backend_id.into() }
    }

    #[test]
    fn listing_pairs_unique_names_with_backend_ids() {
        let l = listing([("X".into(), "a".into()), ("X".into(), "b".into())]);
        assert_eq!(l, vec![PortInfo { index: 0, ..info("X", "X", "a") }, PortInfo { index: 1, ..info("X #2", "X", "b") }]);
    }
}
