// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) 2026 Puffinbox contributors
// Original replacement for the private queue interface consumed by this crate.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

pub(super) struct Queue<T> {
    values: Mutex<VecDeque<T>>,
}

impl<T> Queue<T> {
    pub(super) fn new() -> Self {
        Self { values: Mutex::new(VecDeque::new()) }
    }

    pub(super) fn push(&self, value: T) {
        self.values.lock().unwrap_or_else(PoisonError::into_inner).push_back(value);
    }

    /// Consume one value in insertion order. No user callback runs under the lock.
    ///
    /// # Safety
    /// The surrounding channel protocol requires a single consumer, even though
    /// this queue's own storage is synchronized. Keep that caller contract.
    pub(super) unsafe fn pop_spin(&self) -> Option<T> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner).pop_front()
    }
}
