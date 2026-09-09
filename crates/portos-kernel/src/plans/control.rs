//! Wake a suspended interpreter with either new consent or an abort.

use crate::consent::ConsentRecord;
use std::sync::{Condvar, Mutex};

pub(super) enum RunSignal {
    Resume(ConsentRecord),
    Abort { expired: bool },
}

#[derive(Default)]
pub(super) struct RunControl {
    signal: Mutex<Option<RunSignal>>,
    changed: Condvar,
}

impl RunControl {
    pub(super) fn resume(&self, consent: ConsentRecord) {
        let mut signal = self.signal.lock().unwrap();
        // Once an abort is pending, later consent cannot replace it.
        if !matches!(*signal, Some(RunSignal::Abort { .. })) {
            *signal = Some(RunSignal::Resume(consent));
        }
        self.changed.notify_all();
    }

    pub(super) fn abort(&self, expired: bool) {
        *self.signal.lock().unwrap() = Some(RunSignal::Abort { expired });
        self.changed.notify_all();
    }

    pub(super) fn wait(&self) -> RunSignal {
        let mut signal = self.signal.lock().unwrap();
        loop {
            if let Some(signal) = signal.take() {
                return signal;
            }
            signal = self.changed.wait(signal).unwrap();
        }
    }
}
