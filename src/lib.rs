#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cell::UnsafeCell;
use std::ptr;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

const OFFER_EMPTY: u32 = 0;
const OFFER_OFFERED: u32 = 1;
const OFFER_CLAIMED: u32 = 2;

// Head values contain a 32-bit generation followed by a 32-bit slot index.
// u32::MAX is reserved as the empty-list index, so the maximum representable
// capacity is u32::MAX. Every successful head transition increments the
// generation, preventing a stale observation from claiming a recycled slot.
const EMPTY_INDEX: u32 = u32::MAX;

#[derive(Debug)]
struct Slot {
    next: AtomicU32,
    length: AtomicUsize,
}

// The lifecycle word carries a reuse generation and an explicit state. The
// slot reference and publication sequence live beside it so the full u32 slot
// index range remains available without sacrificing the ABA tag.
struct OfferCell {
    state: AtomicU64,
    slot: AtomicU32,
    sequence: AtomicU64,
}

struct StackState {
    capacity: usize,
    max_value_size: usize,
    slots: Box<[Slot]>,
    // This is allocated once at creation and never resized. Slot ownership,
    // established by the two tagged heads, makes the individual byte ranges
    // safe to access through this cell without an operation-time lock.
    storage: UnsafeCell<Box<[u8]>>,
    free_head: AtomicU64,
    published_head: AtomicU64,
    offer_cells: Box<[OfferCell]>,
    offer_sequence: AtomicU64,
    #[cfg(test)]
    test_pause_after_offer: AtomicBool,
    #[cfg(test)]
    test_offer_published: AtomicBool,
}

// The byte arena is shared, but a slot's state machine gives exactly one
// thread write/read ownership of a slot's byte range at a time:
// free -> reserved -> published -> claimed -> free. Atomic head transitions
// and release/acquire ordering establish the hand-offs between those states.
unsafe impl Sync for StackState {}

pub struct Stack {
    producer_count: u32,
    consumer_count: u32,
    state: Arc<StackState>,
}

pub struct Producer {
    state: Arc<StackState>,
}

pub struct Consumer {
    state: Arc<StackState>,
}

impl Stack {
    fn new(
        capacity: u64,
        max_value_size: u64,
        producer_count: u32,
        consumer_count: u32,
    ) -> Result<Self, u32> {
        if capacity == 0 || max_value_size == 0 || producer_count == 0 || consumer_count == 0 {
            return Err(STATUS_INVALID);
        }

        let capacity = usize::try_from(capacity).map_err(|_| STATUS_INVALID)?;
        let max_value_size = usize::try_from(max_value_size).map_err(|_| STATUS_INVALID)?;
        let capacity_index = u32::try_from(capacity).map_err(|_| STATUS_INVALID)?;
        let producer_count_usize = usize::try_from(producer_count).map_err(|_| STATUS_INVALID)?;
        let consumer_count_usize = usize::try_from(consumer_count).map_err(|_| STATUS_INVALID)?;
        let offer_cell_count = producer_count_usize
            .checked_add(consumer_count_usize)
            .ok_or(STATUS_INTERNAL_ERROR)?;
        let storage_size = capacity
            .checked_mul(max_value_size)
            .ok_or(STATUS_INTERNAL_ERROR)?;

        let mut storage = Vec::new();
        storage
            .try_reserve_exact(storage_size)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        storage.resize(storage_size, 0);

        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for index in 0..capacity_index {
            let next = if index + 1 < capacity_index {
                index + 1
            } else {
                EMPTY_INDEX
            };
            slots.push(Slot {
                next: AtomicU32::new(next),
                length: AtomicUsize::new(0),
            });
        }

        let mut offer_cells = Vec::new();
        offer_cells
            .try_reserve_exact(offer_cell_count)
            .map_err(|_| STATUS_INTERNAL_ERROR)?;
        for _ in 0..offer_cell_count {
            offer_cells.push(OfferCell {
                state: AtomicU64::new(pack_offer_state(0, OFFER_EMPTY)),
                slot: AtomicU32::new(EMPTY_INDEX),
                sequence: AtomicU64::new(0),
            });
        }

        Ok(Self {
            producer_count,
            consumer_count,
            state: Arc::new(StackState {
                capacity,
                max_value_size,
                slots: slots.into_boxed_slice(),
                storage: UnsafeCell::new(storage.into_boxed_slice()),
                free_head: AtomicU64::new(pack_head(0, 0)),
                published_head: AtomicU64::new(pack_head(0, EMPTY_INDEX)),
                offer_cells: offer_cells.into_boxed_slice(),
                offer_sequence: AtomicU64::new(0),
                #[cfg(test)]
                test_pause_after_offer: AtomicBool::new(false),
                #[cfg(test)]
                test_offer_published: AtomicBool::new(false),
            }),
        })
    }

    fn producer(&self, id: u32) -> Result<Producer, u32> {
        (id < self.producer_count)
            .then(|| Producer {
                state: Arc::clone(&self.state),
            })
            .ok_or(STATUS_INVALID)
    }

    fn consumer(&self, id: u32) -> Result<Consumer, u32> {
        (id < self.consumer_count)
            .then(|| Consumer {
                state: Arc::clone(&self.state),
            })
            .ok_or(STATUS_INVALID)
    }
}

impl StackState {
    #[inline]
    fn slot(&self, index: u32) -> &Slot {
        debug_assert!((index as usize) < self.capacity);
        // All indices come from a head initialized from capacity and are
        // checked by the tagged-head algorithm before they reach this point.
        &self.slots[index as usize]
    }

    #[inline]
    unsafe fn slot_data_ptr(&self, index: u32) -> *mut u8 {
        let offset = (index as usize) * self.max_value_size;
        let storage = unsafe { &mut *self.storage.get() };
        unsafe { storage.as_mut_ptr().add(offset) }
    }

    fn claim_free_slot(&self) -> Result<u32, u32> {
        loop {
            let current = self.free_head.load(Ordering::Acquire);
            let (tag, index) = unpack_head(current);
            if index == EMPTY_INDEX {
                return Err(STATUS_FULL);
            }

            let next = self.slot(index).next.load(Ordering::Relaxed);
            let replacement = pack_head(tag.wrapping_add(1), next);
            if self
                .free_head
                .compare_exchange_weak(
                    current,
                    replacement,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(index);
            }
        }
    }

    fn publish_slot(&self, index: u32) {
        loop {
            let current = self.published_head.load(Ordering::Acquire);
            let (_, current_index) = unpack_head(current);
            self.slot(index).next.store(current_index, Ordering::Relaxed);
            let (tag, _) = unpack_head(current);
            let replacement = pack_head(tag.wrapping_add(1), index);
            if self
                .published_head
                .compare_exchange_weak(
                    current,
                    replacement,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // This successful publication CAS is the push
                // linearization point for the withdrawal/fallback path.
                return;
            }
        }
    }

    fn claim_published_slot(&self, output_capacity: usize) -> Result<(u32, usize), u32> {
        loop {
            let current = self.published_head.load(Ordering::Acquire);
            let (tag, index) = unpack_head(current);
            if index == EMPTY_INDEX {
                return Err(STATUS_EMPTY);
            }

            let slot = self.slot(index);
            let length = slot.length.load(Ordering::Relaxed);
            if length > output_capacity {
                // No head or slot state is changed on this path. A concurrent
                // pop may still remove the value, which is linearizable after
                // this failed validation.
                return Err(STATUS_INVALID);
            }

            let next = slot.next.load(Ordering::Relaxed);
            let replacement = pack_head(tag.wrapping_add(1), next);
            if self
                .published_head
                .compare_exchange_weak(
                    current,
                    replacement,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok((index, length));
            }
        }
    }

    fn recycle_slot(&self, index: u32) {
        loop {
            let current = self.free_head.load(Ordering::Acquire);
            let (tag, current_index) = unpack_head(current);
            self.slot(index).next.store(current_index, Ordering::Relaxed);
            let replacement = pack_head(tag.wrapping_add(1), index);
            if self
                .free_head
                .compare_exchange_weak(
                    current,
                    replacement,
                    Ordering::Release,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    // A producer owns a cell while its state is CLAIMED. It fills the tagged
    // slot reference and sequence, then release-publishes OFFERED. If the
    // producer wins the immediate OFFERED -> CLAIMED CAS, the withdrawal
    // publishes the same slot through the global stack; that publication CAS
    // is the push linearization point. If the consumer wins the CAS, that
    // single handoff claim orders the push immediately before the pop, and the
    // consumer owns the slot while it copies it out. Thus OFFERED is a
    // visibility state, not a completed push that can outrank a global item.
    fn offer_or_publish(&self, index: u32) {
        for cell in self.offer_cells.iter() {
            let empty = cell.state.load(Ordering::Acquire);
            let (tag, state) = unpack_offer_state(empty);
            if state != OFFER_EMPTY {
                continue;
            }

            let claim_tag = tag.wrapping_add(1);
            let claimed = pack_offer_state(claim_tag, OFFER_CLAIMED);
            if cell
                .state
                .compare_exchange_weak(
                    empty,
                    claimed,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }

            let sequence = self.offer_sequence.fetch_add(1, Ordering::Relaxed);
            cell.slot.store(index, Ordering::Relaxed);
            cell.sequence.store(sequence, Ordering::Relaxed);

            // Only the producer that reserved the cell writes this state.
            // The release store publishes both the slot reference and the
            // completed value copy to a consumer that acquires OFFERED.
            let offered = pack_offer_state(claim_tag, OFFER_OFFERED);
            cell.state.store(offered, Ordering::Release);

            #[cfg(test)]
            if self.test_pause_after_offer.load(Ordering::Acquire) {
                self.test_offer_published.store(true, Ordering::Release);
                while self.test_pause_after_offer.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }

            if cell
                .state
                .compare_exchange(
                    offered,
                    claimed,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.publish_slot(index);
                cell.state.store(
                    pack_offer_state(claim_tag, OFFER_EMPTY),
                    Ordering::Release,
                );
                return;
            }

            // The consumer won the claim and now owns the slot. It will
            // recycle the slot and release the cell after copying.
            return;
        }

        self.publish_slot(index);
    }

    // Offers are examined only after a global published-head attempt found
    // no item. The newest sequence is the top among concurrently active
    // offers; its successful claim is the direct push/pop linearization pair.
    // A failed claim means another actor won the cell; the caller performs at
    // most one bounded retry through the global path.
    fn claim_offer(
        &self,
        output_capacity: usize,
    ) -> Result<Option<(usize, u32, usize, u32)>, u32> {
        let mut newest: Option<(usize, u64, u32, u32)> = None;
        for (cell_index, cell) in self.offer_cells.iter().enumerate() {
            let state_word = cell.state.load(Ordering::Acquire);
            let (tag, state) = unpack_offer_state(state_word);
            if state != OFFER_OFFERED {
                continue;
            }

            let sequence = cell.sequence.load(Ordering::Relaxed);
            if newest.as_ref().map_or(true, |(_, newest_sequence, _, _)| {
                sequence > *newest_sequence
            }) {
                let index = cell.slot.load(Ordering::Relaxed);
                newest = Some((cell_index, sequence, index, tag));
            }
        }

        let Some((cell_index, _, index, tag)) = newest else {
            return Ok(None);
        };
        let cell = &self.offer_cells[cell_index];
        let offered = pack_offer_state(tag, OFFER_OFFERED);

        // A competing consumer or withdrawing producer may have changed the
        // cell after the scan. Reconfirming the tagged state before reading
        // length prevents an invalid result based on a recycled slot.
        if cell.state.load(Ordering::Acquire) != offered {
            return Ok(None);
        }
        let length = self.slot(index).length.load(Ordering::Relaxed);
        if cell.state.load(Ordering::Acquire) != offered {
            return Ok(None);
        }
        if length > output_capacity {
            return Err(STATUS_INVALID);
        }

        let claimed = pack_offer_state(tag, OFFER_CLAIMED);
        if cell
            .state
            .compare_exchange(
                offered,
                claimed,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(None);
        }

        Ok(Some((cell_index, index, length, tag)))
    }

    fn release_offer(&self, cell_index: usize, tag: u32) {
        let cell = &self.offer_cells[cell_index];
        let claimed = pack_offer_state(tag, OFFER_CLAIMED);
        let empty = pack_offer_state(tag, OFFER_EMPTY);
        debug_assert_eq!(cell.state.load(Ordering::Acquire), claimed);
        cell.state.store(empty, Ordering::Release);
    }

    #[cfg(test)]
    fn pause_after_offer(&self) {
        self.test_offer_published.store(false, Ordering::Release);
        self.test_pause_after_offer.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn offer_is_published(&self) -> bool {
        self.test_offer_published.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn resume_after_offer(&self) {
        self.test_pause_after_offer.store(false, Ordering::Release);
    }
}

impl Producer {
    fn try_push(&self, data: &[u8]) -> u32 {
        if data.len() > self.state.max_value_size {
            return STATUS_INVALID;
        }

        let index = match self.state.claim_free_slot() {
            Ok(index) => index,
            Err(status) => return status,
        };

        // The free-list CAS transferred exclusive ownership of this slot to
        // this producer. The copy is complete before the release publication
        // CAS makes the slot visible to consumers.
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.state.slot_data_ptr(index),
                data.len(),
            );
        }
        self.state.slot(index).length.store(data.len(), Ordering::Relaxed);
        self.state.offer_or_publish(index);
        STATUS_OK
    }
}

impl Consumer {
    fn try_pop(&self, output: &mut [u8]) -> Result<usize, u32> {
        match self.state.claim_published_slot(output.len()) {
            Ok((index, length)) => return self.copy_global_claim(output, index, length),
            Err(STATUS_EMPTY) => {}
            Err(status) => return Err(status),
        }

        if let Some((cell_index, index, length, tag)) = self.state.claim_offer(output.len())? {
            unsafe {
                ptr::copy_nonoverlapping(
                    self.state.slot_data_ptr(index),
                    output.as_mut_ptr(),
                    length,
                );
            }
            self.state.recycle_slot(index);
            self.state.release_offer(cell_index, tag);
            return Ok(length);
        }

        // A withdrawing producer may have published between the first empty
        // observation and the bounded offer scan. Check the global path once
        // more; otherwise EMPTY is linearized at one of the empty reads.
        match self.state.claim_published_slot(output.len()) {
            Ok((index, length)) => self.copy_global_claim(output, index, length),
            Err(status) => Err(status),
        }
    }

    fn copy_global_claim(
        &self,
        output: &mut [u8],
        index: u32,
        length: usize,
    ) -> Result<usize, u32> {
        // The publication CAS acquired the producer's completed copy. The
        // successful head claim is the pop linearization point, transfers
        // exclusive ownership before output is written, and recycle occurs
        // only after that copy completes.
        unsafe {
            ptr::copy_nonoverlapping(
                self.state.slot_data_ptr(index),
                output.as_mut_ptr(),
                length,
            );
        }
        self.state.recycle_slot(index);
        Ok(length)
    }
}

#[inline]
fn pack_head(tag: u32, index: u32) -> u64 {
    (u64::from(tag) << 32) | u64::from(index)
}

#[inline]
fn unpack_head(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

#[inline]
fn pack_offer_state(tag: u32, state: u32) -> u64 {
    (u64::from(tag) << 32) | u64::from(state)
}

#[inline]
fn unpack_offer_state(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    #[test]
    fn safe_core_is_bounded_stack() {
        let stack = Stack::new(3, 8, 1, 1).unwrap();
        let producer = stack.producer(0).unwrap();
        let consumer = stack.consumer(0).unwrap();
        assert_eq!(producer.try_push(b"A"), STATUS_OK);
        assert_eq!(producer.try_push(b"B"), STATUS_OK);
        assert_eq!(producer.try_push(b"C"), STATUS_OK);
        assert_eq!(producer.try_push(b"D"), STATUS_FULL);

        let mut output = [0_u8; 8];
        for expected in [b"C".as_slice(), b"B", b"A"] {
            assert_eq!(consumer.try_pop(&mut output), Ok(expected.len()));
            assert_eq!(&output[..expected.len()], expected);
        }
        assert_eq!(consumer.try_pop(&mut output), Err(STATUS_EMPTY));
    }

    #[test]
    fn insufficient_output_does_not_remove_value() {
        let stack = Stack::new(2, 8, 1, 1).unwrap();
        let producer = stack.producer(0).unwrap();
        let consumer = stack.consumer(0).unwrap();
        assert_eq!(producer.try_push(b"value"), STATUS_OK);

        let mut too_small = [0xA5_u8; 4];
        assert_eq!(consumer.try_pop(&mut too_small), Err(STATUS_INVALID));
        assert_eq!(too_small, [0xA5_u8; 4]);

        let mut output = [0_u8; 8];
        assert_eq!(consumer.try_pop(&mut output), Ok(5));
        assert_eq!(&output[..5], b"value");
    }

    #[test]
    fn direct_handoff_claim_and_invalid_output_preserve_offer() {
        let stack = Arc::new(Stack::new(2, 8, 1, 1).unwrap());
        stack.state.pause_after_offer();
        let producer = stack.producer(0).unwrap();
        let producer_thread = thread::spawn(move || producer.try_push(b"value"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !stack.state.offer_is_published() {
            if std::time::Instant::now() >= deadline {
                stack.state.resume_after_offer();
                let _ = producer_thread.join();
                panic!("producer did not publish a handoff offer");
            }
            thread::yield_now();
        }

        let consumer = stack.consumer(0).unwrap();
        let mut too_small = [0xA5_u8; 4];
        assert_eq!(consumer.try_pop(&mut too_small), Err(STATUS_INVALID));
        assert_eq!(too_small, [0xA5_u8; 4]);

        let mut output = [0_u8; 8];
        assert_eq!(consumer.try_pop(&mut output), Ok(5));
        assert_eq!(&output[..5], b"value");
        stack.state.resume_after_offer();
        assert_eq!(producer_thread.join().unwrap(), STATUS_OK);
        assert!(stack
            .state
            .offer_cells
            .iter()
            .all(|cell| unpack_offer_state(cell.state.load(Ordering::Acquire)).1 == OFFER_EMPTY));
        assert_eq!(consumer.try_pop(&mut output), Err(STATUS_EMPTY));
    }

    #[test]
    fn newest_overlapping_offer_is_popped_first() {
        let stack = Arc::new(Stack::new(4, 8, 2, 1).unwrap());
        stack.state.pause_after_offer();
        let producer_a = stack.producer(0).unwrap();
        let producer_b = stack.producer(1).unwrap();
        let thread_a = thread::spawn(move || producer_a.try_push(b"A"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let offered = stack
                .state
                .offer_cells
                .iter()
                .filter(|cell| {
                    unpack_offer_state(cell.state.load(Ordering::Acquire)).1 == OFFER_OFFERED
                })
                .count();
            if offered == 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                stack.state.resume_after_offer();
                let _ = thread_a.join();
                panic!("first producer did not publish a handoff offer");
            }
            thread::yield_now();
        }

        let thread_b = thread::spawn(move || producer_b.try_push(b"B"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let offered = stack
                .state
                .offer_cells
                .iter()
                .filter(|cell| {
                    unpack_offer_state(cell.state.load(Ordering::Acquire)).1 == OFFER_OFFERED
                })
                .count();
            if offered == 2 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                stack.state.resume_after_offer();
                let _ = thread_a.join();
                let _ = thread_b.join();
                panic!("both producers did not publish handoff offers");
            }
            thread::yield_now();
        }

        let consumer = stack.consumer(0).unwrap();
        let mut output = [0_u8; 8];
        assert_eq!(consumer.try_pop(&mut output), Ok(1));
        assert_eq!(&output[..1], b"B");
        assert_eq!(consumer.try_pop(&mut output), Ok(1));
        assert_eq!(&output[..1], b"A");
        stack.state.resume_after_offer();
        assert_eq!(thread_a.join().unwrap(), STATUS_OK);
        assert_eq!(thread_b.join().unwrap(), STATUS_OK);
    }

    #[test]
    fn withdrawal_publishes_and_reuses_offer_cell() {
        let stack = Stack::new(2, 8, 1, 1).unwrap();
        let producer = stack.producer(0).unwrap();
        let consumer = stack.consumer(0).unwrap();

        for value in [b"first".as_slice(), b"second"] {
            assert_eq!(producer.try_push(value), STATUS_OK);
            assert!(stack
                .state
                .offer_cells
                .iter()
                .all(|cell| unpack_offer_state(cell.state.load(Ordering::Acquire)).1 == OFFER_EMPTY));
            let mut output = [0_u8; 8];
            assert_eq!(consumer.try_pop(&mut output), Ok(value.len()));
            assert_eq!(&output[..value.len()], value);
        }
    }

    #[test]
    fn duplicate_payloads_remain_distinct_stack_items() {
        let stack = Stack::new(4, 8, 1, 1).unwrap();
        let producer = stack.producer(0).unwrap();
        let consumer = stack.consumer(0).unwrap();
        assert_eq!(producer.try_push(b"same"), STATUS_OK);
        assert_eq!(producer.try_push(b"same"), STATUS_OK);

        let mut output = [0_u8; 8];
        assert_eq!(consumer.try_pop(&mut output), Ok(4));
        assert_eq!(&output[..4], b"same");
        assert_eq!(consumer.try_pop(&mut output), Ok(4));
        assert_eq!(&output[..4], b"same");
        assert_eq!(consumer.try_pop(&mut output), Err(STATUS_EMPTY));
    }

    #[test]
    fn concurrent_mpmc_conserves_distinct_slots() {
        const PRODUCERS: usize = 4;
        const CONSUMERS: usize = 4;
        const VALUES_PER_PRODUCER: usize = 400;
        const TOTAL: usize = PRODUCERS * VALUES_PER_PRODUCER;

        let stack = Arc::new(Stack::new(64, 8, PRODUCERS as u32, CONSUMERS as u32).unwrap());
        let start = Arc::new(Barrier::new(PRODUCERS + CONSUMERS));
        let popped = Arc::new(AtomicUsize::new(0));
        let values = Arc::new(Mutex::new(Vec::with_capacity(TOTAL)));
        let mut threads = Vec::new();

        for producer_id in 0..PRODUCERS {
            let stack = Arc::clone(&stack);
            let start = Arc::clone(&start);
            threads.push(thread::spawn(move || {
                let producer = stack.producer(producer_id as u32).unwrap();
                start.wait();
                for value_id in 0..VALUES_PER_PRODUCER {
                    let value = ((producer_id as u64) << 32 | value_id as u64).to_le_bytes();
                    loop {
                        match producer.try_push(&value) {
                            STATUS_OK => break,
                            STATUS_FULL => thread::yield_now(),
                            status => panic!("unexpected push status {status}"),
                        }
                    }
                }
            }));
        }

        for consumer_id in 0..CONSUMERS {
            let stack = Arc::clone(&stack);
            let start = Arc::clone(&start);
            let popped = Arc::clone(&popped);
            let values = Arc::clone(&values);
            threads.push(thread::spawn(move || {
                let consumer = stack.consumer(consumer_id as u32).unwrap();
                start.wait();
                let mut output = [0_u8; 8];
                loop {
                    if popped.load(Ordering::Acquire) >= TOTAL {
                        break;
                    }
                    match consumer.try_pop(&mut output) {
                        Ok(length) => {
                            assert_eq!(length, 8);
                            values.lock().unwrap().push(u64::from_le_bytes(output));
                            popped.fetch_add(1, Ordering::Release);
                        }
                        Err(STATUS_EMPTY) => thread::yield_now(),
                        Err(status) => panic!("unexpected pop status {status}"),
                    }
                }
            }));
        }

        for thread in threads {
            thread.join().unwrap();
        }

        let values = values.lock().unwrap();
        assert_eq!(values.len(), TOTAL);
        let unique: HashSet<u64> = values.iter().copied().collect();
        assert_eq!(unique.len(), TOTAL);
        for producer_id in 0..PRODUCERS {
            for value_id in 0..VALUES_PER_PRODUCER {
                assert!(unique.contains(&((producer_id as u64) << 32 | value_id as u64)));
            }
        }
        let mut output = [0_u8; 8];
        assert_eq!(stack.consumer(0).unwrap().try_pop(&mut output), Err(STATUS_EMPTY));
    }
}
