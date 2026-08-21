//! Bounded concurrent capture scheduling and input-order reassembly.

use std::collections::BTreeMap;
use std::sync::{Mutex, mpsc};
use std::thread;

use super::{
    Archiver, CaptureControl, CaptureEvent, CaptureEventSink, CaptureOutcome, Collection, Error,
    notify_outcome,
};

type IndexedOutcome = (usize, String, CaptureOutcome);

impl Archiver {
    /// Capture URLs with a pool of worker threads, recording outcomes in input order.
    pub(super) fn capture_concurrently<I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        concurrency: usize,
        collection: &mut Collection,
        events: &mut impl CaptureEventSink,
    ) -> Result<bool, Error> {
        let mut urls = urls.into_iter();
        let (task_sender, task_receiver) = mpsc::channel::<(usize, String)>();
        let task_receiver = Mutex::new(task_receiver);
        let (outcome_sender, outcome_receiver) = mpsc::sync_channel::<IndexedOutcome>(concurrency);

        thread::scope(|scope| {
            for _ in 0..concurrency {
                let task_receiver = &task_receiver;
                let outcome_sender = outcome_sender.clone();

                scope.spawn(move || {
                    loop {
                        let task = task_receiver
                            .lock()
                            .ok()
                            .and_then(|receiver| receiver.recv().ok());
                        let Some((index, url)) = task else { return };
                        let outcome = self.capture(&url, None);

                        if outcome_sender.send((index, url, outcome)).is_err() {
                            return;
                        }
                    }
                });
            }

            drop(outcome_sender);
            let mut dispatched = 0;
            let mut cancelled = false;
            for (index, url) in urls.by_ref().take(concurrency).enumerate() {
                let url = url.as_ref().to_owned();
                if events.event(CaptureEvent::Started {
                    url: &url,
                    attempt: 1,
                }) == CaptureControl::Cancel
                {
                    cancelled = true;
                    break;
                }
                let _ = task_sender.send((index, url));
                dispatched += 1;
            }

            let mut result = Ok(());
            let mut completed = 0;
            let mut next_to_record = 0;
            let mut pending = BTreeMap::new();

            while completed < dispatched {
                let (index, url, outcome) = outcome_receiver
                    .recv()
                    .expect("workers always report an outcome before exiting");
                let (exchanges, error) = outcome.parts();
                completed += 1;

                if result.is_ok() {
                    cancelled |= notify_outcome(events, &url, &exchanges, error.as_ref());
                    if !cancelled && let Some(url) = urls.next() {
                        let url = url.as_ref().to_owned();
                        if events.event(CaptureEvent::Started {
                            url: &url,
                            attempt: 1,
                        }) == CaptureControl::Cancel
                        {
                            cancelled = true;
                        } else {
                            let _ = task_sender.send((dispatched, url));
                            dispatched += 1;
                        }
                    }
                    pending.insert(index, (url, exchanges, error));
                    while let Some((url, exchanges, error)) = pending.remove(&next_to_record) {
                        if let Err(error) =
                            collection.record(url.clone(), exchanges, error, None, None)
                        {
                            result = Err(error);
                            break;
                        }
                        cancelled |= events.event(CaptureEvent::Written { url: &url })
                            == CaptureControl::Cancel;
                        next_to_record += 1;
                    }
                }
            }

            drop(task_sender);
            result.map(|()| cancelled)
        })
    }
}
