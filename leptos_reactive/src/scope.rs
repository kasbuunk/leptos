#![forbid(unsafe_code)]
use crate::{
    console_warn,
    hydration::FragmentData,
    node::NodeId,
    runtime::{with_runtime, RuntimeId},
    suspense::StreamChunk,
    PinnedFuture, ResourceId, SpecialNonReactiveZone, StoredValueId,
    SuspenseContext,
};
use futures::stream::FuturesUnordered;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
};

#[doc(hidden)]
#[must_use = "Scope will leak memory if the disposer function is never called"]
/// Creates a new reactive system and root reactive scope and runs the function within it.
///
/// This should usually only be used once, at the root of an application, because its reactive
/// values will not have access to values created under another `create_scope`.
///
/// You usually don't need to call this manually.
pub fn create_scope(
    runtime: RuntimeId,
    f: impl FnOnce(Scope) + 'static,
) -> ScopeDisposer {
    runtime.run_scope_undisposed(f, None).2
}

#[doc(hidden)]
#[must_use = "Scope will leak memory if the disposer function is never called"]
/// Creates a new reactive system and root reactive scope, and returns them.
///
/// This should usually only be used once, at the root of an application, because its reactive
/// values will not have access to values created under another `create_scope`.
///
/// You usually don't need to call this manually.
#[cfg_attr(
    any(debug_assertions, features = "ssr"),
    instrument(level = "trace", skip_all,)
)]
pub fn raw_scope_and_disposer(runtime: RuntimeId) -> (Scope, ScopeDisposer) {
    runtime.raw_scope_and_disposer()
}

#[doc(hidden)]
/// Creates a temporary scope, runs the given function, disposes of the scope,
/// and returns the value returned from the function. This is very useful for short-lived
/// applications like SSR, where actual reactivity is not required beyond the end
/// of the synchronous operation.
///
/// You usually don't need to call this manually.
#[cfg_attr(
    any(debug_assertions, features = "ssr"),
    instrument(level = "trace", skip_all,)
)]
pub fn run_scope<T>(
    runtime: RuntimeId,
    f: impl FnOnce(Scope) -> T + 'static,
) -> T {
    runtime.run_scope(f, None)
}

#[doc(hidden)]
#[must_use = "Scope will leak memory if the disposer function is never called"]
/// Creates a temporary scope and run the given function without disposing of the scope.
/// If you do not dispose of the scope on your own, memory will leak.
///
/// You usually don't need to call this manually.
#[cfg_attr(
    any(debug_assertions, features = "ssr"),
    instrument(level = "trace", skip_all,)
)]
pub fn run_scope_undisposed<T>(
    runtime: RuntimeId,
    f: impl FnOnce(Scope) -> T + 'static,
) -> (T, ScopeId, ScopeDisposer) {
    runtime.run_scope_undisposed(f, None)
}

/// A Each scope can have
/// child scopes, and may in turn have a parent.
///
/// Scopes manage memory within the reactive system. When a scope is disposed, its
/// cleanup functions run and the signals, effects, memos, resources, and contexts
/// associated with it no longer exist and should no longer be accessed.
///
/// You generally won’t need to create your own scopes when writing application code.
/// However, they’re very useful for managing control flow within an application or library.
/// For example, if you are writing a keyed list component, you will want to create a child scope
/// for each row in the list so that you can dispose of its associated signals, etc.
/// when it is removed from the list.
///
/// Every other function in this crate takes a `Scope` as its first argument. Since `Scope`
/// is [`Copy`] and `'static` this does not add much overhead or lifetime complexity.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    #[doc(hidden)]
    pub runtime: RuntimeId,
    #[doc(hidden)]
    pub id: ScopeId,
}

impl Scope {
    /// The unique identifier for this scope.
    pub fn id(&self) -> ScopeId {
        self.id
    }

    /// Creates a child scope and runs the given function within it, returning a handle to dispose of it.
    ///
    /// The child scope has its own lifetime and disposer, but will be disposed when the parent is
    /// disposed, if it has not been already.
    ///
    /// This is useful for applications like a list or a router, which may want to create child scopes and
    /// dispose of them when they are no longer needed (e.g., a list item has been destroyed or the user
    /// has navigated away from the route.)
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[inline(always)]
    pub fn child_scope(self, f: impl FnOnce(Scope)) -> ScopeDisposer {
        let (_, disposer) = self.run_child_scope(f);
        disposer
    }

    /// Creates a child scope and runs the given function within it, returning the function's return
    /// type and a handle to dispose of it.
    ///
    /// The child scope has its own lifetime and disposer, but will be disposed when the parent is
    /// disposed, if it has not been already.
    ///
    /// This is useful for applications like a list or a router, which may want to create child scopes and
    /// dispose of them when they are no longer needed (e.g., a list item has been destroyed or the user
    /// has navigated away from the route.)
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[inline(always)]
    pub fn run_child_scope<T>(
        self,
        f: impl FnOnce(Scope) -> T,
    ) -> (T, ScopeDisposer) {
        let (res, child_id, disposer) =
            self.runtime.run_scope_undisposed(f, Some(self));

        (res, disposer)
    }

    /// Suspends reactive tracking while running the given function.
    ///
    /// This can be used to isolate parts of the reactive graph from one another.
    ///
    /// ```
    /// # use leptos_reactive::*;
    /// # run_scope(create_runtime(), |cx| {
    /// let (a, set_a) = create_signal(cx, 0);
    /// let (b, set_b) = create_signal(cx, 0);
    /// let c = create_memo(cx, move |_| {
    ///     // this memo will *only* update when `a` changes
    ///     a() + cx.untrack(move || b())
    /// });
    ///
    /// assert_eq!(c(), 0);
    /// set_a(1);
    /// assert_eq!(c(), 1);
    /// set_b(1);
    /// // hasn't updated, because we untracked before reading b
    /// assert_eq!(c(), 1);
    /// set_a(2);
    /// assert_eq!(c(), 3);
    ///
    /// # });
    /// ```
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[inline(always)]
    pub fn untrack<T>(&self, f: impl FnOnce() -> T) -> T {
        with_runtime(self.runtime, |runtime| {
            let untracked_result;

            SpecialNonReactiveZone::enter();

            let prev_observer =
                SetObserverOnDrop(self.runtime, runtime.observer.take());

            untracked_result = f();

            runtime.observer.set(prev_observer.1);
            std::mem::forget(prev_observer); // avoid Drop

            SpecialNonReactiveZone::exit();

            untracked_result
        })
        .expect(
            "tried to run untracked function in a runtime that has been \
             disposed",
        )
    }
}

struct SetObserverOnDrop(RuntimeId, Option<NodeId>);

impl Drop for SetObserverOnDrop {
    fn drop(&mut self) {
        _ = with_runtime(self.0, |rt| {
            rt.observer.set(self.1);
        });
    }
}

// Internals

impl Scope {
    /// Disposes of this reactive scope.
    ///
    /// This will
    /// 1. dispose of all child `Scope`s
    /// 2. run all cleanup functions defined for this scope by [`on_cleanup`](crate::on_cleanup).
    /// 3. dispose of all signals, effects, and resources owned by this `Scope`.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn dispose(self) {
        _ = with_runtime(self.runtime, |runtime| {})
    }
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[track_caller]
    pub(crate) fn push_scope_property(&self, prop: ScopeProperty) {
        #[cfg(debug_assertions)]
        let defined_at = std::panic::Location::caller();
        _ = with_runtime(self.runtime, |runtime| {
            runtime.register_property(
                prop,
                #[cfg(debug_assertions)]
                defined_at,
            );
        })
    }
}

#[cfg_attr(
    any(debug_assertions, features = "ssr"),
    instrument(level = "trace", skip_all,)
)]
fn push_cleanup(cx: Scope, cleanup_fn: Box<dyn FnOnce()>) {
    _ = with_runtime(cx.runtime, |runtime| {
        if let Some(owner) = runtime.owner.get() {
            let mut cleanups = runtime.on_cleanups.borrow_mut();
            if let Some(entries) = cleanups.get_mut(owner) {
                entries.push(cleanup_fn);
            } else {
                cleanups.insert(owner, vec![cleanup_fn]);
            }
        }
    });
}

/// Creates a cleanup function, which will be run when a [`Scope`] is disposed.
///
/// It runs after child scopes have been disposed, but before signals, effects, and resources
/// are invalidated.
#[inline(always)]
pub fn on_cleanup(cx: Scope, cleanup_fn: impl FnOnce() + 'static) {
    push_cleanup(cx, Box::new(cleanup_fn))
}

slotmap::new_key_type! {
    /// Unique ID assigned to a [`Scope`](crate::Scope).
    pub struct ScopeId;
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ScopeProperty {
    Trigger(NodeId),
    Signal(NodeId),
    Effect(NodeId),
    Resource(ResourceId),
    StoredValue(StoredValueId),
}

impl ScopeProperty {
    pub fn to_node_id(self) -> Option<NodeId> {
        match self {
            Self::Trigger(node) | Self::Signal(node) | Self::Effect(node) => {
                Some(node)
            }
            _ => None,
        }
    }
}

/// Creating a [`Scope`](crate::Scope) gives you a disposer, which can be called
/// to dispose of that reactive scope.
///
/// This will
/// 1. dispose of all child `Scope`s
/// 2. run all cleanup functions defined for this scope by [`on_cleanup`](crate::on_cleanup).
/// 3. dispose of all signals, effects, and resources owned by this `Scope`.
#[repr(transparent)]
pub struct ScopeDisposer(pub(crate) Scope);

impl ScopeDisposer {
    /// Disposes of a reactive [`Scope`](crate::Scope).
    ///
    /// This will
    /// 1. dispose of all child `Scope`s
    /// 2. run all cleanup functions defined for this scope by [`on_cleanup`](crate::on_cleanup).
    /// 3. dispose of all signals, effects, and resources owned by this `Scope`.
    #[inline(always)]
    pub fn dispose(self) {
        self.0.dispose()
    }
}

impl Scope {
    /// Returns IDs for all [`Resource`](crate::Resource)s found on any scope.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn all_resources(&self) -> Vec<ResourceId> {
        with_runtime(self.runtime, |runtime| runtime.all_resources())
            .unwrap_or_default()
    }

    /// Returns IDs for all [`Resource`](crate::Resource)s found on any scope that are
    /// pending from the server.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn pending_resources(&self) -> Vec<ResourceId> {
        with_runtime(self.runtime, |runtime| runtime.pending_resources())
            .unwrap_or_default()
    }

    /// Returns IDs for all [`Resource`](crate::Resource)s found on any scope.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn serialization_resolvers(
        &self,
    ) -> FuturesUnordered<PinnedFuture<(ResourceId, String)>> {
        with_runtime(self.runtime, |runtime| {
            runtime.serialization_resolvers(*self)
        })
        .unwrap_or_default()
    }

    /// Registers the given [`SuspenseContext`](crate::SuspenseContext) with the current scope,
    /// calling the `resolver` when its resources are all resolved.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn register_suspense(
        &self,
        context: SuspenseContext,
        key: &str,
        out_of_order_resolver: impl FnOnce() -> String + 'static,
        in_order_resolver: impl FnOnce() -> VecDeque<StreamChunk> + 'static,
    ) {
        use crate::create_isomorphic_effect;
        use futures::StreamExt;

        _ = with_runtime(self.runtime, |runtime| {
            let mut shared_context = runtime.shared_context.borrow_mut();
            let (tx1, mut rx1) = futures::channel::mpsc::unbounded();
            let (tx2, mut rx2) = futures::channel::mpsc::unbounded();
            let (tx3, mut rx3) = futures::channel::mpsc::unbounded();

            create_isomorphic_effect(*self, move |_| {
                let pending = context
                    .pending_serializable_resources
                    .read_only()
                    .try_with(|n| *n)
                    .unwrap_or(0);
                if pending == 0 {
                    _ = tx1.unbounded_send(());
                    _ = tx2.unbounded_send(());
                    _ = tx3.unbounded_send(());
                }
            });

            shared_context.pending_fragments.insert(
                key.to_string(),
                FragmentData {
                    out_of_order: Box::pin(async move {
                        rx1.next().await;
                        out_of_order_resolver()
                    }),
                    in_order: Box::pin(async move {
                        rx2.next().await;
                        in_order_resolver()
                    }),
                    should_block: context.should_block(),
                    is_ready: Some(Box::pin(async move {
                        rx3.next().await;
                    })),
                },
            );
        })
    }

    /// The set of all HTML fragments currently pending.
    ///
    /// The keys are hydration IDs. Values are tuples of two pinned
    /// `Future`s that return content for out-of-order and in-order streaming, respectively.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn pending_fragments(&self) -> HashMap<String, FragmentData> {
        with_runtime(self.runtime, |runtime| {
            let mut shared_context = runtime.shared_context.borrow_mut();
            std::mem::take(&mut shared_context.pending_fragments)
        })
        .unwrap_or_default()
    }

    /// A future that will resolve when all blocking fragments are ready.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn blocking_fragments_ready(self) -> PinnedFuture<()> {
        use futures::StreamExt;

        let mut ready = with_runtime(self.runtime, |runtime| {
            let mut shared_context = runtime.shared_context.borrow_mut();
            let ready = FuturesUnordered::new();
            for (_, data) in shared_context.pending_fragments.iter_mut() {
                if data.should_block {
                    if let Some(is_ready) = data.is_ready.take() {
                        ready.push(is_ready);
                    }
                }
            }
            ready
        })
        .unwrap_or_default();
        Box::pin(async move { while ready.next().await.is_some() {} })
    }

    /// Takes the pending HTML for a single `<Suspense/>` node.
    ///
    /// Returns a tuple of two pinned `Future`s that return content for out-of-order
    /// and in-order streaming, respectively.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub fn take_pending_fragment(&self, id: &str) -> Option<FragmentData> {
        with_runtime(self.runtime, |runtime| {
            let mut shared_context = runtime.shared_context.borrow_mut();
            shared_context.pending_fragments.remove(id)
        })
        .ok()
        .flatten()
    }

    /// Batches any reactive updates, preventing effects from running until the whole
    /// function has run. This allows you to prevent rerunning effects if multiple
    /// signal updates might cause the same effect to run.
    ///
    /// # Panics
    /// Panics if the runtime this scope belongs to has already been disposed.
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[inline(always)]
    pub fn batch<T>(&self, f: impl FnOnce() -> T) -> T {
        with_runtime(self.runtime, move |runtime| {
            let batching =
                SetBatchingOnDrop(self.runtime, runtime.batching.get());
            runtime.batching.set(true);

            let val = f();

            runtime.batching.set(batching.1);
            std::mem::forget(batching);

            runtime.run_effects();
            val
        })
        .expect(
            "tried to run a batched update in a runtime that has been disposed",
        )
    }
}

struct SetBatchingOnDrop(RuntimeId, bool);

impl Drop for SetBatchingOnDrop {
    fn drop(&mut self) {
        _ = with_runtime(self.0, |rt| {
            rt.batching.set(self.1);
        });
    }
}

impl fmt::Debug for ScopeDisposer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ScopeDisposer").finish()
    }
}
