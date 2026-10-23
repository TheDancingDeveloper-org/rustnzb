//! Per-server connection budget shared by every consumer of a provider.
//!
//! A provider account allows `connections` sockets in total. The downloader
//! and the WebDAV side each keep their own sockets, but both take a
//! [`BudgetPermit`] from the same [`ServerBudget`] before opening one, so
//! together they never exceed the allowance.
//!
//! Permits are handed out by tier, highest first:
//!
//! 1. [`BudgetTier::Playback`]: a demand read for media someone is watching.
//! 2. [`BudgetTier::Download`]: the downloader, DAV ingest and read-ahead.
//! 3. [`BudgetTier::Repair`]: repairing a gap in a file.
//!
//! Two floors protect the lower-capacity tiers:
//!
//! - The **playback floor** is set aside: Download and Repair never take the
//!   last `playback_floor - held(Playback)` free permits. A download permit
//!   is held for a worker's lifetime and cannot be preempted, so playback is
//!   only responsive if its share is kept free in advance.
//! - The **repair floor** is guaranteed against Download while repair is
//!   active (a repair permit is held or one is waited for). Repair gets
//!   freed permits ahead of Download until it holds its floor, and Download
//!   leaves the unmet part of the floor free. With no repair work the floor
//!   costs Download nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Default share of a server's connections set aside for playback when
/// WebDAV streaming is enabled.
pub const DEFAULT_PLAYBACK_RESERVE_PCT: u8 = 25;

/// Default share of a server's connections guaranteed to repair against
/// download and read-ahead (zurg's quarter).
pub const DEFAULT_REPAIR_RESERVE_PCT: u8 = 25;

/// Who is asking for a connection. Earlier variants have higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetTier {
    /// A read for media being played right now.
    Playback,
    /// Bulk download, DAV ingest and read-ahead.
    Download,
    /// Repair of a missing or damaged range.
    Repair,
}

impl BudgetTier {
    const COUNT: usize = 3;

    fn idx(self) -> usize {
        match self {
            BudgetTier::Playback => 0,
            BudgetTier::Download => 1,
            BudgetTier::Repair => 2,
        }
    }
}

/// How a server's connections are split between the tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPolicy {
    /// Percent of `connections` set aside for playback. 0 when WebDAV
    /// streaming is off.
    pub playback_reserve_pct: u8,
    /// Percent of `connections` guaranteed to repair against download.
    pub repair_reserve_pct: u8,
}

impl Default for BudgetPolicy {
    /// No playback reserve (nothing streams unless WebDAV is enabled) and
    /// the default repair floor.
    fn default() -> Self {
        Self {
            playback_reserve_pct: 0,
            repair_reserve_pct: DEFAULT_REPAIR_RESERVE_PCT,
        }
    }
}

impl BudgetPolicy {
    /// `(playback_floor, repair_floor)` for a server with `total` connections.
    ///
    /// Each floor is the percentage rounded up, so a reserve that is asked
    /// for is never rounded away. The floors are capped so the download tier
    /// always keeps at least one connection, and a one-connection server has
    /// no floors at all.
    pub fn floors(&self, total: usize) -> (usize, usize) {
        if total < 2 {
            return (0, 0);
        }
        let max = total - 1;
        let pct = |p: u8| (total * usize::from(p.min(100))).div_ceil(100);
        let playback = pct(self.playback_reserve_pct).min(max);
        let repair = pct(self.repair_reserve_pct).min(max - playback);
        (playback, repair)
    }
}

struct State {
    total: usize,
    playback_floor: usize,
    repair_floor: usize,
    held: [usize; BudgetTier::COUNT],
    waiters: [VecDeque<oneshot::Sender<BudgetPermit>>; BudgetTier::COUNT],
}

impl State {
    fn held_total(&self) -> usize {
        self.held.iter().sum()
    }

    fn free(&self) -> usize {
        self.total.saturating_sub(self.held_total())
    }

    fn waiting(&self, tier: BudgetTier) -> bool {
        !self.waiters[tier.idx()].is_empty()
    }

    fn unmet_playback(&self) -> usize {
        self.playback_floor
            .saturating_sub(self.held[BudgetTier::Playback.idx()])
    }

    fn unmet_repair(&self) -> usize {
        let repair = BudgetTier::Repair;
        let active = self.held[repair.idx()] > 0 || self.waiting(repair);
        if active {
            self.repair_floor.saturating_sub(self.held[repair.idx()])
        } else {
            0
        }
    }

    /// The tier whose first waiter should get the next permit, if any can.
    fn next_grantable(&self) -> Option<BudgetTier> {
        let free = self.free();
        if free == 0 {
            return None;
        }
        let unmet_playback = self.unmet_playback();
        if self.waiting(BudgetTier::Playback) {
            return Some(BudgetTier::Playback);
        }
        let repair_below_floor = self.held[BudgetTier::Repair.idx()] < self.repair_floor;
        if self.waiting(BudgetTier::Repair) && repair_below_floor && free > unmet_playback {
            return Some(BudgetTier::Repair);
        }
        if self.waiting(BudgetTier::Download) && free > unmet_playback + self.unmet_repair() {
            return Some(BudgetTier::Download);
        }
        if self.waiting(BudgetTier::Repair) && free > unmet_playback {
            return Some(BudgetTier::Repair);
        }
        None
    }
}

/// The connection allowance for one server, shared by every consumer.
pub struct ServerBudget {
    state: Mutex<State>,
}

impl ServerBudget {
    /// A budget of `total` permits split by `policy`.
    pub fn new(total: usize, policy: BudgetPolicy) -> Arc<Self> {
        let (playback_floor, repair_floor) = policy.floors(total);
        Arc::new(Self {
            state: Mutex::new(State {
                total,
                playback_floor,
                repair_floor,
                held: [0; BudgetTier::COUNT],
                waiters: Default::default(),
            }),
        })
    }

    /// Wait for a permit at `tier`. Dropping the future gives up the place
    /// in line; a permit already granted to it is returned.
    pub async fn acquire(self: &Arc<Self>, tier: BudgetTier) -> BudgetPermit {
        loop {
            let rx = {
                let mut state = self.state.lock();
                let (tx, rx) = oneshot::channel();
                state.waiters[tier.idx()].push_back(tx);
                self.dispatch(&mut state);
                rx
            };
            // The sender is only dropped without sending when the waiter is
            // pruned as closed, which cannot happen while `rx` is alive.
            if let Ok(permit) = rx.await {
                return permit;
            }
        }
    }

    /// Change the allowance. Permits already held stay valid; when the
    /// total shrinks below what is held, no new permit is granted until
    /// enough are returned.
    pub fn set_total(self: &Arc<Self>, total: usize, policy: BudgetPolicy) {
        let mut state = self.state.lock();
        let (playback_floor, repair_floor) = policy.floors(total);
        state.total = total;
        state.playback_floor = playback_floor;
        state.repair_floor = repair_floor;
        self.dispatch(&mut state);
    }

    /// The configured allowance.
    pub fn total(&self) -> usize {
        self.state.lock().total
    }

    /// Permits currently held at `tier`.
    pub fn held(&self, tier: BudgetTier) -> usize {
        self.state.lock().held[tier.idx()]
    }

    /// Permits currently held across all tiers.
    pub fn held_total(&self) -> usize {
        self.state.lock().held_total()
    }

    /// Whether a consumer at a tier other than `tier` is waiting for a
    /// permit. A pool uses this to close a socket rather than park it idle,
    /// so its permit passes to the waiter.
    pub fn others_waiting(&self, tier: BudgetTier) -> bool {
        let state = self.state.lock();
        [
            BudgetTier::Playback,
            BudgetTier::Download,
            BudgetTier::Repair,
        ]
        .into_iter()
        .filter(|t| *t != tier)
        .any(|t| state.waiters[t.idx()].iter().any(|tx| !tx.is_closed()))
    }

    /// `(playback_floor, repair_floor)` in permits.
    pub fn floors(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.playback_floor, state.repair_floor)
    }

    /// Hand free permits to waiters, highest priority first.
    fn dispatch(self: &Arc<Self>, state: &mut State) {
        for queue in &mut state.waiters {
            queue.retain(|tx| !tx.is_closed());
        }
        while let Some(tier) = state.next_grantable() {
            let Some(tx) = state.waiters[tier.idx()].pop_front() else {
                break;
            };
            state.held[tier.idx()] += 1;
            let permit = BudgetPermit {
                budget: Some(Arc::clone(self)),
                tier,
            };
            if let Err(mut permit) = tx.send(permit) {
                // The waiter left between the prune and the send. Undo by
                // hand: dropping the permit would re-lock the state.
                permit.budget = None;
                state.held[tier.idx()] -= 1;
            }
        }
    }

    fn release(self: &Arc<Self>, tier: BudgetTier) {
        let mut state = self.state.lock();
        state.held[tier.idx()] -= 1;
        self.dispatch(&mut state);
    }
}

/// One unit of a server's allowance. Returned to the budget on drop.
pub struct BudgetPermit {
    budget: Option<Arc<ServerBudget>>,
    tier: BudgetTier,
}

impl BudgetPermit {
    /// The tier this permit was granted at.
    pub fn tier(&self) -> BudgetTier {
        self.tier
    }
}

impl std::fmt::Debug for BudgetPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetPermit")
            .field("tier", &self.tier)
            .finish()
    }
}

impl Drop for BudgetPermit {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.take() {
            budget.release(self.tier);
        }
    }
}

/// The budgets of every configured server, keyed by server id, and the
/// policy that splits them. One registry is shared by the downloader and
/// the WebDAV side.
#[derive(Default)]
pub struct BudgetRegistry {
    policy: Mutex<BudgetPolicy>,
    budgets: Mutex<HashMap<String, Arc<ServerBudget>>>,
}

impl BudgetRegistry {
    pub fn new(policy: BudgetPolicy) -> Self {
        Self {
            policy: Mutex::new(policy),
            budgets: Mutex::new(HashMap::new()),
        }
    }

    pub fn policy(&self) -> BudgetPolicy {
        *self.policy.lock()
    }

    /// Change the split for every server, now and for servers added later.
    pub fn set_policy(&self, policy: BudgetPolicy) {
        *self.policy.lock() = policy;
        for budget in self.budgets.lock().values() {
            let total = budget.total();
            budget.set_total(total, policy);
        }
    }

    /// The budget for `server_id`, created on first use, with its allowance
    /// set to `connections`.
    pub fn ensure(&self, server_id: &str, connections: usize) -> Arc<ServerBudget> {
        let policy = self.policy();
        let mut budgets = self.budgets.lock();
        match budgets.get(server_id) {
            Some(budget) => {
                budget.set_total(connections, policy);
                Arc::clone(budget)
            }
            None => {
                let budget = ServerBudget::new(connections, policy);
                budgets.insert(server_id.to_string(), Arc::clone(&budget));
                budget
            }
        }
    }

    /// The budget for `server_id`, if one exists.
    pub fn get(&self, server_id: &str) -> Option<Arc<ServerBudget>> {
        self.budgets.lock().get(server_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SHORT: Duration = Duration::from_millis(50);

    fn policy(playback: u8, repair: u8) -> BudgetPolicy {
        BudgetPolicy {
            playback_reserve_pct: playback,
            repair_reserve_pct: repair,
        }
    }

    async fn try_acquire(budget: &Arc<ServerBudget>, tier: BudgetTier) -> Option<BudgetPermit> {
        tokio::time::timeout(SHORT, budget.acquire(tier)).await.ok()
    }

    #[test]
    fn floors_round_up_and_leave_download_one() {
        assert_eq!(policy(25, 25).floors(20), (5, 5));
        assert_eq!(policy(25, 25).floors(10), (3, 3));
        assert_eq!(policy(25, 25).floors(2), (1, 0));
        assert_eq!(policy(25, 25).floors(1), (0, 0));
        assert_eq!(policy(100, 100).floors(8), (7, 0));
        assert_eq!(policy(0, 25).floors(8), (0, 2));
    }

    #[tokio::test]
    async fn download_leaves_playback_floor_free() {
        let budget = ServerBudget::new(8, policy(25, 0));
        let mut held = Vec::new();
        for _ in 0..6 {
            held.push(try_acquire(&budget, BudgetTier::Download).await.unwrap());
        }
        assert!(
            try_acquire(&budget, BudgetTier::Download).await.is_none(),
            "download took the playback reserve"
        );
        let p1 = try_acquire(&budget, BudgetTier::Playback).await.unwrap();
        let p2 = try_acquire(&budget, BudgetTier::Playback).await.unwrap();
        assert!(try_acquire(&budget, BudgetTier::Playback).await.is_none());
        assert_eq!(budget.held_total(), 8);
        drop((p1, p2));
    }

    #[tokio::test]
    async fn playback_is_served_before_queued_downloads() {
        let budget = ServerBudget::new(4, policy(0, 0));
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(budget.acquire(BudgetTier::Download).await);
        }
        let download = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { budget.acquire(BudgetTier::Download).await }
        });
        tokio::time::sleep(SHORT).await;
        let playback = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { budget.acquire(BudgetTier::Playback).await }
        });
        tokio::time::sleep(SHORT).await;

        held.pop();
        let permit = tokio::time::timeout(SHORT * 4, playback)
            .await
            .expect("playback waited behind a download")
            .unwrap();
        assert_eq!(permit.tier(), BudgetTier::Playback);
        assert!(!download.is_finished());
        drop(permit);
        let permit = tokio::time::timeout(SHORT * 4, download)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(permit.tier(), BudgetTier::Download);
    }

    #[tokio::test]
    async fn repair_floor_holds_against_download() {
        let budget = ServerBudget::new(8, policy(0, 25));
        // No repair work: download may use everything.
        let mut downloads = Vec::new();
        for _ in 0..8 {
            downloads.push(try_acquire(&budget, BudgetTier::Download).await.unwrap());
        }
        // Repair asks while download holds all eight.
        let repair = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move {
                let a = budget.acquire(BudgetTier::Repair).await;
                let b = budget.acquire(BudgetTier::Repair).await;
                (a, b)
            }
        });
        tokio::time::sleep(SHORT).await;
        // A download waiter queued before the freed permits.
        let more_download = tokio::spawn({
            let budget = Arc::clone(&budget);
            async move { budget.acquire(BudgetTier::Download).await }
        });
        tokio::time::sleep(SHORT).await;

        downloads.pop();
        downloads.pop();
        let (a, b) = tokio::time::timeout(SHORT * 4, repair)
            .await
            .expect("repair floor was not honoured")
            .unwrap();
        assert_eq!(budget.held(BudgetTier::Repair), 2);
        assert!(!more_download.is_finished());

        // While repair holds one permit, download keeps the floor's other
        // permit free for it.
        drop(b);
        tokio::time::sleep(SHORT).await;
        assert!(
            !more_download.is_finished(),
            "download took the repair floor"
        );
        drop(a);
        tokio::time::timeout(SHORT * 4, more_download)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn never_exceeds_total_under_concurrent_load() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let budget = ServerBudget::new(6, policy(25, 25));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for i in 0..60 {
            let tier = [
                BudgetTier::Playback,
                BudgetTier::Download,
                BudgetTier::Repair,
            ][i % 3];
            let (budget, live, peak) = (Arc::clone(&budget), Arc::clone(&live), Arc::clone(&peak));
            tasks.push(tokio::spawn(async move {
                for _ in 0..20 {
                    let _permit = budget.acquire(tier).await;
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("a tier starved")
                .unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 6);
        assert_eq!(budget.held_total(), 0);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_leak_a_permit() {
        let budget = ServerBudget::new(1, policy(0, 0));
        let held = budget.acquire(BudgetTier::Download).await;
        assert!(try_acquire(&budget, BudgetTier::Download).await.is_none());
        drop(held);
        assert_eq!(budget.held_total(), 0);
        let _again = try_acquire(&budget, BudgetTier::Download).await.unwrap();
    }

    #[tokio::test]
    async fn shrink_counts_held_permits_until_returned() {
        let budget = ServerBudget::new(4, policy(0, 0));
        let a = budget.acquire(BudgetTier::Download).await;
        let b = budget.acquire(BudgetTier::Download).await;
        budget.set_total(1, policy(0, 0));
        assert!(try_acquire(&budget, BudgetTier::Playback).await.is_none());
        drop(a);
        assert!(try_acquire(&budget, BudgetTier::Playback).await.is_none());
        drop(b);
        assert!(try_acquire(&budget, BudgetTier::Playback).await.is_some());
    }

    #[test]
    fn registry_shares_one_budget_per_server_and_applies_policy() {
        let registry = BudgetRegistry::default();
        let a = registry.ensure("srv", 8);
        let b = registry.ensure("srv", 8);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.floors(), (0, 2));
        registry.set_policy(policy(25, 25));
        assert_eq!(a.floors(), (2, 2));
        registry.ensure("srv", 12);
        assert_eq!(a.total(), 12);
        assert_eq!(a.floors(), (3, 3));
    }
}
