//! Port identity and the open-port table shared by [`crate::MidiInputs`] and [`crate::FakeSource`].
//!
//! A device has two names:
//! - its **backend id** (midir's `MidiInputPort::id()`; the device-interface path on WinMM), stable
//!   for the same device in the same socket and distinct for two identical devices;
//! - its **listed name**, the human name made unique within one listing by a " #2", " #3" suffix.
//!   The suffix only disambiguates within that listing: when the first of two identical controllers
//!   is unplugged, the second one's listed name changes from "X #2" to "X". Persist mappings on
//!   name plus backend id, not on the suffixed name alone.
//!
//! [`PortId`]s are keyed on the backend id, so a device keeps its id across renames and replugs.

use std::collections::HashSet;

use crate::{MidiIoError, PortId, PortInfo};

/// A change in the device list, reported by [`crate::MidiSource::refresh`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortChange {
    /// A device appeared (not opened automatically).
    Appeared { name: String, backend_id: String },
    /// A device that was not open went away.
    Disappeared { name: String, backend_id: String },
    /// An open port's device went away or its connection died; the port has been closed. Release
    /// any notes still held from it.
    Closed { port: PortId, name: String },
    /// An open port's listed name changed (a duplicate ahead of it went away). Same device, same id.
    Renamed { port: PortId, from: String, to: String },
}

/// Port id by backend id, with each port's latest listed name. Stable for the life of a source.
#[derive(Debug, Default)]
pub(crate) struct PortIds {
    /// `(backend_id, name)`; the index is the id.
    entries: Vec<(String, String)>,
}

impl PortIds {
    /// The id for a device, created on first sight; records `name` as its current name.
    pub(crate) fn id_for(&mut self, backend_id: &str, name: &str) -> PortId {
        if let Some(i) = self.entries.iter().position(|(b, _)| b == backend_id) {
            if let Some((_, n)) = self.entries.get_mut(i) {
                name.clone_into(n);
            }
            return PortId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        self.entries.push((backend_id.to_string(), name.to_string()));
        PortId(u32::try_from(self.entries.len().saturating_sub(1)).unwrap_or(u32::MAX))
    }

    fn entry(&self, id: PortId) -> Option<&(String, String)> {
        self.entries.get(usize::try_from(id.0).ok()?)
    }

    pub(crate) fn name(&self, id: PortId) -> Option<&str> {
        self.entry(id).map(|(_, n)| n.as_str())
    }

    pub(crate) fn backend_id(&self, id: PortId) -> Option<&str> {
        self.entry(id).map(|(b, _)| b.as_str())
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
    let (names, ids): (Vec<String>, Vec<String>) = devices.into_iter().unzip();
    unique_names(names).into_iter().zip(ids).enumerate().map(|(index, (name, backend_id))| PortInfo { name, index, backend_id }).collect()
}

/// One open connection.
pub(crate) struct OpenPort<C> {
    pub(crate) id: PortId,
    pub(crate) name: String,
    pub(crate) backend_id: String,
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
        let listed = |backend_id: &str| now.iter().find(|p| p.backend_id == backend_id);
        let mut kept = Vec::with_capacity(self.open.len());
        let mut closed: Vec<String> = Vec::new();
        for mut p in self.open.drain(..) {
            match listed(&p.backend_id) {
                Some(info) if alive(&p.conn) => {
                    if info.name != p.name {
                        let from = std::mem::replace(&mut p.name, info.name.clone());
                        self.pending.push(PortChange::Renamed { port: p.id, from, to: info.name.clone() });
                    }
                    kept.push(p);
                }
                // Dropping `p` closes the dead connection.
                _ => {
                    closed.push(p.backend_id);
                    self.pending.push(PortChange::Closed { port: p.id, name: p.name });
                }
            }
        }
        self.open = kept;
        for p in &self.open {
            self.ids.id_for(&p.backend_id, &p.name);
        }
        let before = self.known.take().unwrap_or_default();
        let is_open = |b: &str| self.open.iter().any(|p| p.backend_id == b);
        for p in now.iter().filter(|p| !before.iter().any(|q| q.backend_id == p.backend_id)) {
            self.pending.push(PortChange::Appeared { name: p.name.clone(), backend_id: p.backend_id.clone() });
        }
        // Open ones that vanished were reported as `Closed` above.
        let gone = |q: &&PortInfo| !now.iter().any(|p| p.backend_id == q.backend_id) && !is_open(&q.backend_id) && !closed.contains(&q.backend_id);
        for q in before.iter().filter(gone) {
            self.pending.push(PortChange::Disappeared { name: q.name.clone(), backend_id: q.backend_id.clone() });
        }
        self.known = Some(now.to_vec());
    }

    /// Reconciles with `now` and returns every change since the last call.
    pub(crate) fn refresh(&mut self, now: &[PortInfo], alive: impl Fn(&C) -> bool) -> Vec<PortChange> {
        self.reconcile(now, alive);
        std::mem::take(&mut self.pending)
    }

    /// Opens the port listed as `name` in `now`. If its device is already open (under any name) and
    /// alive, returns that port's id without connecting again; a dead connection is replaced.
    pub(crate) fn open(
        &mut self,
        now: &[PortInfo],
        name: &str,
        alive: impl Fn(&C) -> bool,
        connect: impl FnOnce(PortId, &PortInfo) -> Result<C, MidiIoError>,
    ) -> Result<PortId, MidiIoError> {
        self.reconcile(now, alive);
        let info = now.iter().find(|p| p.name == name).ok_or_else(|| MidiIoError::NoSuchPort(name.to_string()))?;
        if let Some(p) = self.open.iter().find(|p| p.backend_id == info.backend_id) {
            return Ok(p.id);
        }
        let id = self.ids.id_for(&info.backend_id, &info.name);
        let conn = connect(id, info)?;
        self.open.push(OpenPort { id, name: info.name.clone(), backend_id: info.backend_id.clone(), conn });
        Ok(id)
    }

    /// Removes the port open as `name`, returning its connection to close.
    pub(crate) fn close(&mut self, name: &str) -> Option<C> {
        let i = self.open.iter().position(|p| p.name == name)?;
        Some(self.open.swap_remove(i).conn)
    }

    /// Closes everything (connections drop in order).
    pub(crate) fn close_all(&mut self) {
        self.open.clear();
    }

    pub(crate) fn is_open(&self, name: &str) -> bool {
        self.open.iter().any(|p| p.name == name)
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut OpenPort<C>> {
        self.open.iter_mut().find(|p| p.name == name)
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut OpenPort<C>> {
        self.open.iter_mut()
    }

    pub(crate) fn port_name(&self, id: PortId) -> Option<&str> {
        self.ids.name(id)
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
    fn port_ids_follow_the_backend_id() {
        let mut ids = PortIds::default();
        let a = ids.id_for("usb-a", "X");
        let b = ids.id_for("usb-b", "X #2");
        assert_ne!(a, b);
        // Same device, renamed: same id, new name.
        assert_eq!(ids.id_for("usb-b", "X"), b);
        assert_eq!(ids.name(b), Some("X"));
        assert_eq!(ids.backend_id(b), Some("usb-b"));
        assert_eq!(ids.name(PortId(99)), None);
    }

    #[test]
    fn listing_pairs_unique_names_with_backend_ids() {
        let l = listing([("X".into(), "a".into()), ("X".into(), "b".into())]);
        assert_eq!(
            l,
            vec![PortInfo { name: "X".into(), index: 0, backend_id: "a".into() }, PortInfo { name: "X #2".into(), index: 1, backend_id: "b".into() }]
        );
    }
}
