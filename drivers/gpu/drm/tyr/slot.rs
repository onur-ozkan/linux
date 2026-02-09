// SPDX-License-Identifier: GPL-2.0 or MIT

//! Slot management abstraction for limited hardware resources.
//!
//! This module provides a generic [`SlotManager`] that assigns limited hardware
//! slots to logical "seats". A seat represents an entity (such as a vm address
//! space) that needs access to a hardware slot.
//!
//! The [`SlotManager`] tracks slot allocation using sequence numbers to detect
//! when a seat's binding has been invalidated. When a seat requests activation,
//! the manager will either reuse the seat's existing slot (if still valid),
//! allocate a free slot (if any are available), or evict the oldest idle slot if any
//! slots are idle.
//!
//! Hardware-specific behavior is customized by implementing the [`SlotOperations`]
//! trait, which allows callbacks when slots are activated or evicted.
//!
//! This is primarily used for managing address space slots in the GPU, where
//! the number of hardware address space slots is limited.
//!
//! [SlotOperations]: crate::slot::SlotOperations
//! [SlotManager]: crate::slot::SlotManager
#![allow(dead_code)]

use core::{
    mem::take,
    ops::{
        Deref,
        DerefMut, //
    }, //
};

use kernel::{
    prelude::*,
    sync::LockedBy, //
};

pub(crate) struct SeatInfo {
    /// Slot used by this seat
    slot: u8,

    /// Sequence number encoding the last time this seat was active.
    /// We also use it to check if a slot is still bound to a seat.
    seqno: u64,
}

#[derive(Default)]
pub(crate) enum Seat {
    #[expect(clippy::enum_variant_names)]
    #[default]
    NoSeat,
    Active(SeatInfo),
    Idle(SeatInfo),
}

impl Seat {
    pub(super) fn slot(&self) -> Option<u8> {
        match self {
            Self::Active(info) => Some(info.slot),
            _ => None,
        }
    }
}

pub(crate) trait SlotOperations {
    type SlotData;

    /// Called when a slot is being activated for a seat.
    ///
    /// This callback allows hardware-specific actions to be performed when a slot
    /// becomes active, such as updating hardware registers or invalidating caches.
    fn activate(&mut self, _slot_idx: usize, _slot_data: &Self::SlotData) -> Result {
        Ok(())
    }

    /// Called when a slot is being evicted and freed.
    ///
    /// This callback allows hardware-specific cleanup when a slot is being
    /// completely freed, either explicitly or when an idle slot is being
    /// reused for a different seat. Any hardware state should be invalidated.
    fn evict(&mut self, _slot_idx: usize, _slot_data: &Self::SlotData) -> Result {
        Ok(())
    }
}

struct SlotInfo<T> {
    /// Type specific data attached to a slot
    slot_data: T,

    /// Sequence number from when this slot was last activated
    seqno: u64,
}

#[derive(Default)]
enum Slot<T> {
    #[default]
    Free,
    Active(SlotInfo<T>),
    Idle(SlotInfo<T>),
}

pub(crate) struct SlotManager<T: SlotOperations, const MAX_SLOTS: usize> {
    /// Manager specific data
    manager: T,

    /// Number of slots actually available
    slot_count: usize,

    /// Slots
    slots: [Slot<T::SlotData>; MAX_SLOTS],

    /// Sequence number incremented each time a Seat is successfully activated
    use_seqno: u64,
}

// A `Seat` protected by the same lock that is used to wrap the `SlotManager`.
type LockedSeat<T, const MAX_SLOTS: usize> = LockedBy<Seat, SlotManager<T, MAX_SLOTS>>;

impl<T: SlotOperations, const MAX_SLOTS: usize> Unpin for SlotManager<T, MAX_SLOTS> {}

impl<T: SlotOperations, const MAX_SLOTS: usize> SlotManager<T, MAX_SLOTS> {
    pub(crate) fn new(manager: T, slot_count: usize) -> Result<Self> {
        if slot_count == 0 {
            return Err(EINVAL);
        }
        if slot_count > MAX_SLOTS {
            return Err(EINVAL);
        }
        Ok(Self {
            manager,
            slot_count,
            slots: [const { Slot::Free }; MAX_SLOTS],
            use_seqno: 1,
        })
    }

    fn record_active_slot(
        &mut self,
        slot_idx: usize,
        locked_seat: &LockedSeat<T, MAX_SLOTS>,
        slot_data: T::SlotData,
    ) -> Result {
        let cur_seqno = self.use_seqno;

        *locked_seat.access_mut(self) = Seat::Active(SeatInfo {
            slot: slot_idx as u8,
            seqno: cur_seqno,
        });

        self.slots[slot_idx] = Slot::Active(SlotInfo {
            slot_data,
            seqno: cur_seqno,
        });

        self.use_seqno += 1;
        Ok(())
    }

    fn activate_slot(
        &mut self,
        slot_idx: usize,
        locked_seat: &LockedSeat<T, MAX_SLOTS>,
        slot_data: T::SlotData,
    ) -> Result {
        self.manager.activate(slot_idx, &slot_data)?;
        self.record_active_slot(slot_idx, locked_seat, slot_data)
    }

    fn allocate_slot(
        &mut self,
        locked_seat: &LockedSeat<T, MAX_SLOTS>,
        slot_data: T::SlotData,
    ) -> Result {
        let slots = &self.slots[..self.slot_count];

        let mut idle_slot_idx = None;
        let mut idle_slot_seqno: u64 = 0;

        for (slot_idx, slot) in slots.iter().enumerate() {
            match slot {
                Slot::Free => {
                    return self.activate_slot(slot_idx, locked_seat, slot_data);
                }
                Slot::Idle(slot_info) => {
                    if idle_slot_idx.is_none() || slot_info.seqno < idle_slot_seqno {
                        idle_slot_idx = Some(slot_idx);
                        idle_slot_seqno = slot_info.seqno;
                    }
                }
                Slot::Active(_) => (),
            }
        }

        match idle_slot_idx {
            Some(slot_idx) => {
                // Lazily evict idle slot just before it is reused
                if let Slot::Idle(slot_info) = &self.slots[slot_idx] {
                    self.manager.evict(slot_idx, &slot_info.slot_data)?;
                }
                self.activate_slot(slot_idx, locked_seat, slot_data)
            }
            None => {
                pr_err!(
                    "Slot allocation failed: all {} slots in use\n",
                    self.slot_count
                );
                Err(EBUSY)
            }
        }
    }

    fn idle_slot(&mut self, slot_idx: usize, locked_seat: &LockedSeat<T, MAX_SLOTS>) -> Result {
        let slot = take(&mut self.slots[slot_idx]);

        if let Slot::Active(slot_info) = slot {
            self.slots[slot_idx] = Slot::Idle(SlotInfo {
                slot_data: slot_info.slot_data,
                seqno: slot_info.seqno,
            })
        };

        *locked_seat.access_mut(self) = match locked_seat.access(self) {
            Seat::Active(seat_info) | Seat::Idle(seat_info) => Seat::Idle(SeatInfo {
                slot: seat_info.slot,
                seqno: seat_info.seqno,
            }),
            Seat::NoSeat => Seat::NoSeat,
        };
        Ok(())
    }

    fn evict_slot(&mut self, slot_idx: usize, locked_seat: &LockedSeat<T, MAX_SLOTS>) -> Result {
        match &self.slots[slot_idx] {
            Slot::Active(slot_info) | Slot::Idle(slot_info) => {
                self.manager.evict(slot_idx, &slot_info.slot_data)?;
                take(&mut self.slots[slot_idx]);
            }
            _ => (),
        }

        *locked_seat.access_mut(self) = Seat::NoSeat;
        Ok(())
    }

    // Checks and updates the seat state based on the slot it points to
    // (if any). Returns a Seat with a value matching the slot state.
    fn check_seat(&mut self, locked_seat: &LockedSeat<T, MAX_SLOTS>) -> Seat {
        let new_seat = match locked_seat.access(self) {
            Seat::Active(seat_info) => {
                let old_slot_idx = seat_info.slot as usize;
                let slot = &self.slots[old_slot_idx];

                if kernel::warn_on!(
                    !matches!(slot, Slot::Active(slot_info) if slot_info.seqno == seat_info.seqno)
                ) {
                    Seat::NoSeat
                } else {
                    Seat::Active(SeatInfo {
                        slot: seat_info.slot,
                        seqno: seat_info.seqno,
                    })
                }
            }

            Seat::Idle(seat_info) => {
                let old_slot_idx = seat_info.slot as usize;
                let slot = &self.slots[old_slot_idx];

                if !matches!(slot, Slot::Idle(slot_info) if slot_info.seqno == seat_info.seqno) {
                    Seat::NoSeat
                } else {
                    Seat::Idle(SeatInfo {
                        slot: seat_info.slot,
                        seqno: seat_info.seqno,
                    })
                }
            }

            _ => Seat::NoSeat,
        };

        // FIXME: Annoying manual copy. The original idea was to not add Copy+Clone to SeatInfo,
        // so that only slot.rs can change the seat state, but there might be better solutions
        // to prevent that.
        match &new_seat {
            Seat::Active(seat_info) => {
                *locked_seat.access_mut(self) = Seat::Active(SeatInfo {
                    slot: seat_info.slot,
                    seqno: seat_info.seqno,
                })
            }
            Seat::Idle(seat_info) => {
                *locked_seat.access_mut(self) = Seat::Idle(SeatInfo {
                    slot: seat_info.slot,
                    seqno: seat_info.seqno,
                })
            }
            _ => *locked_seat.access_mut(self) = Seat::NoSeat,
        }

        new_seat
    }

    pub(crate) fn activate(
        &mut self,
        locked_seat: &LockedSeat<T, MAX_SLOTS>,
        slot_data: T::SlotData,
    ) -> Result {
        let seat = self.check_seat(locked_seat);
        match seat {
            Seat::Active(seat_info) | Seat::Idle(seat_info) => {
                // With lazy eviction, if seqno matches, the hardware state is still
                // valid for both Active and Idle slots, so just update our records
                self.record_active_slot(seat_info.slot as usize, locked_seat, slot_data)
            }
            _ => self.allocate_slot(locked_seat, slot_data),
        }
    }

    // The idle() method will be used when we start adding support for user VMs
    #[expect(dead_code)]
    pub(crate) fn idle(&mut self, locked_seat: &LockedSeat<T, MAX_SLOTS>) -> Result {
        let seat = self.check_seat(locked_seat);
        if let Seat::Active(seat_info) = seat {
            self.idle_slot(seat_info.slot as usize, locked_seat)?;
        }
        Ok(())
    }

    /// Evict a seat from its slot, freeing up the hardware resource.
    pub(crate) fn evict(&mut self, locked_seat: &LockedSeat<T, MAX_SLOTS>) -> Result {
        let seat = self.check_seat(locked_seat);

        match seat {
            Seat::Active(seat_info) | Seat::Idle(seat_info) => {
                let slot_idx = seat_info.slot as usize;

                self.evict_slot(slot_idx, locked_seat)?;
            }
            _ => (),
        }

        Ok(())
    }
}

impl<T: SlotOperations, const MAX_SLOTS: usize> Deref for SlotManager<T, MAX_SLOTS> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.manager
    }
}

impl<T: SlotOperations, const MAX_SLOTS: usize> DerefMut for SlotManager<T, MAX_SLOTS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.manager
    }
}
