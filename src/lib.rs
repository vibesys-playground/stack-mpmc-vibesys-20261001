#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;

use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

const ABI_VERSION: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_FULL: u32 = 1;
const STATUS_EMPTY: u32 = 2;
const STATUS_INVALID: u32 = 3;
const STATUS_INTERNAL_ERROR: u32 = 4;

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
        self.state.publish_slot(index);
        STATUS_OK
    }
}

impl Consumer {
    fn try_pop(&self, output: &mut [u8]) -> Result<usize, u32> {
        let (index, length) = self.state.claim_published_slot(output.len())?;

        // The publication CAS acquired the producer's completed copy. The
        // successful claim transfers exclusive ownership before output is
        // written, and recycle occurs only after that copy completes.
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
