use std::fmt;
use std::sync::Arc;

use crate::OperationId;

/// Identity recorded immediately after a provider process is spawned.
///
/// The receipt arrives before provider output can be observed. A durable host
/// can persist it beside its operation or lease identity, then reconcile the
/// process after a worker restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SpawnReceipt {
    /// The provider service that launched the child.
    pub provider: &'static str,
    /// The host-local operation whose attempt launched the child.
    pub operation_id: OperationId,
    /// The direct provider child's process id.
    pub pid: u32,
    /// The provider process group id when the child leads an owned group.
    ///
    /// `None` means the child shares its caller's process group. In that case
    /// the pid must not be passed to a process-group kill operation.
    pub process_group_id: Option<u32>,
}

impl SpawnReceipt {
    pub const fn new(
        provider: &'static str,
        operation_id: OperationId,
        pid: u32,
        process_group_id: Option<u32>,
    ) -> Self {
        Self {
            provider,
            operation_id,
            pid,
            process_group_id,
        }
    }
}

/// Host-local callback for durable provider process registration.
///
/// The callback runs inline on the provider spawning thread and must not
/// block. Prefer a nonblocking channel or a bounded local write. Delivery is
/// per process, so a provider-level retry may produce multiple receipts for
/// one operation.
#[derive(Clone)]
pub struct SpawnObserver(Arc<dyn Fn(SpawnReceipt) + Send + Sync>);

impl SpawnObserver {
    pub fn new(observer: impl Fn(SpawnReceipt) + Send + Sync + 'static) -> Self {
        Self(Arc::new(observer))
    }

    pub fn observe(&self, receipt: SpawnReceipt) {
        (self.0)(receipt);
    }
}

impl fmt::Debug for SpawnObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SpawnObserver").field(&"..").finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn cloned_observers_share_the_same_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let observer = SpawnObserver::new(move |receipt| sink.lock().unwrap().push(receipt));

        let receipt = SpawnReceipt::new("fake", OperationId::from_u64(7), 17, Some(17));
        observer.clone().observe(receipt);

        assert_eq!(*seen.lock().unwrap(), [receipt]);
    }
}
