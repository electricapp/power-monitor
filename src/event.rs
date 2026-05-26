//! Diagnostic event hook for long-running samplers.
//!
//! Register a callback via [`set_event_hook`] to receive notifications when
//! the sampler encounters an unusual condition — for instance, the IOReport
//! channel schema changing at runtime, or a null delta dictionary.
//!
//! The hook is a plain `fn` pointer — no allocation, no trait object, and
//! zero overhead when no hook is registered (single relaxed atomic load per
//! sample). It is safe to set or clear the hook from any thread.
//!
//! # Example
//!
//! ```
//! use power_monitor::{set_event_hook, SamplerEvent};
//!
//! fn log_event(event: &SamplerEvent) {
//!     eprintln!("[power-monitor] {event:?}");
//! }
//!
//! set_event_hook(Some(log_event));
//! // ... run sampler ...
//! set_event_hook(None);
//! ```

use std::sync::atomic::{AtomicPtr, Ordering};

/// Diagnostic events emitted by the sampler at runtime.
///
/// Register a receiver via [`set_event_hook`]. Events are emitted best-effort
/// on the sampling thread — keep the handler fast and non-blocking.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SamplerEvent {
    /// The IOReport channel set was populated or changed.
    ///
    /// Fired on the first sample (`previous == 0`) and any subsequent
    /// sample where the channel count differs from the cached scratch. A
    /// reshuffle implies the per-channel state-name cache was rebuilt, so
    /// the next sample path reallocates.
    SchemaChanged {
        /// Previous channel count (0 on the first sample).
        previous: usize,
        /// New channel count observed in the delta dictionary.
        current: usize,
    },
    /// `IOReportCreateSamplesDelta` returned null. The sample was skipped
    /// and cached values are stale until the next successful delta.
    NullDelta,
}

/// Callback signature for a registered event hook.
pub type EventHook = fn(&SamplerEvent);

// AtomicPtr<()> stores the raw fn pointer. Fn pointers are always
// pointer-sized and non-null, so we use null as "no hook registered".
static EVENT_HOOK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Install a diagnostic event hook. Pass `None` to clear.
///
/// Setting a hook is cheap and thread-safe. The hook is invoked on the
/// sampling thread; if it needs to publish to another thread, do so via
/// a channel or atomic flag — do not block.
pub fn set_event_hook(hook: Option<EventHook>) {
    let ptr = match hook {
        Some(f) => f as *mut (),
        None => std::ptr::null_mut(),
    };
    // Relaxed: a fn pointer carries its own meaning — no other data is
    // published through this atomic that the caller would need to observe.
    EVENT_HOOK.store(ptr, Ordering::Relaxed);
}

/// Emit an event to the registered hook, if any.
///
/// Called from sampler internals. Costs one relaxed atomic load when no
/// hook is set, which optimises to nothing measurable on modern CPUs.
#[inline]
pub(crate) fn emit(event: SamplerEvent) {
    let ptr = EVENT_HOOK.load(Ordering::Relaxed);
    if ptr.is_null() {
        return;
    }
    // SAFETY: ptr was stored via `set_event_hook` from a valid `fn(&SamplerEvent)`
    // pointer. Fn pointers have the same ABI as `*const ()` in Rust.
    let hook: EventHook = unsafe { std::mem::transmute::<*mut (), EventHook>(ptr) };
    hook(&event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counting_hook(_e: &SamplerEvent) {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn hook_roundtrip() {
        CALLS.store(0, Ordering::Relaxed);
        set_event_hook(Some(counting_hook));
        emit(SamplerEvent::NullDelta);
        emit(SamplerEvent::SchemaChanged {
            previous: 0,
            current: 42,
        });
        assert_eq!(CALLS.load(Ordering::Relaxed), 2);

        set_event_hook(None);
        emit(SamplerEvent::NullDelta);
        assert_eq!(CALLS.load(Ordering::Relaxed), 2, "hook should be cleared");
    }
}
