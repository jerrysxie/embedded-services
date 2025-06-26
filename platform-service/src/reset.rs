//! API for managing software controlled CPU/MCU resets

use core::future::Future;

use embassy_sync::{lazy_lock::LazyLock, signal::Signal};

use embedded_services::{intrusive_list, GlobalRawMutex, IntrusiveList, Node, NodeContainer};

static BLOCKERS: LazyLock<IntrusiveList> = LazyLock::new(IntrusiveList::new);

pub struct Blocker {
    node: Node,
    reset_pending: Signal<GlobalRawMutex, ()>,
    unblocked: Signal<GlobalRawMutex, ()>,
}

impl NodeContainer for Blocker {
    fn get_node(&self) -> &Node {
        &self.node
    }
}

impl Blocker {
    /// allocate a Blocker, such that it could be used in a static
    pub const fn uninit() -> Self {
        Self {
            node: Node::uninit(),
            reset_pending: Signal::new(),
            unblocked: Signal::new(),
        }
    }

    /// call once on startup to be registered as a Reset handling blocker, forwards any error states (such as double registration) from intrusive_list
    pub async fn register(&'static self) -> intrusive_list::Result<()> {
        BLOCKERS.get().push(self)
    }

    /// waitable reset indicator, for handling resets
    pub async fn wait_for_reset<F, Fut>(&self, before_reset: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        self.reset_pending.wait().await;
        before_reset().await;
        self.unblocked.signal(());
    }
}

/// Signals and waits for all registered blockers to complete their async operations before performing a platform-specific reset, typically NVIC_RESET
pub async fn system_reset() -> ! {
    // signal and wait for completion as two separate events to allow for alternative scheduling algorithms to take effect
    let blockers = BLOCKERS.get();

    // 1. signal all events
    for blocker in blockers.iter_only::<Blocker>() {
        blocker.reset_pending.signal(());
    }

    // 2. wait for all events
    for blocker in blockers.iter_only::<Blocker>() {
        blocker.unblocked.wait().await;
    }

    // 3. perform platform reset
    #[cfg(feature = "cortex-m")]
    cortex_m::peripheral::SCB::sys_reset();

    // no equivalent reset option for std environment
    #[cfg(not(feature = "cortex-m"))]
    panic!("Cannot reset without NVIC");
}
