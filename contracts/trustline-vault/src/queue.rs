// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Withdrawal queue layer (FIFO) plugged on top of the vault’s internal exit.
//!
//! Storage: one **persistent** entry per request + instance `head` / `tail` /
//! `len` (no single 64 KiB vector rewrite). Cancel unlinks in O(n) walk.
//!
//! Public `withdraw` / `redeem`:
//! - **Fast path** — queue empty: enqueue then [`process`] once (settles this
//!   request immediately when `available_liquidity` covers payout + fee).
//! - **Slow path** — queue already non-empty: enqueue only; never flush the
//!   backlog. Settlement is via [`process`] / `process_withdrawals`.
//!
//! Requests must be at least [`MIN_WITHDRAWAL_SHARES`] shares.
//! The request owner or the optional [`manager`] may [`cancel`] to leave the
//! queue (shares + cost restored to the request owner).
//! While the queue is non-empty, the vault must not auto-allocate deposits to the
//! liquidity adapter ([`is_empty`]).

use crate::fees::{FeeHost, FeeLib, Sale};
use soroban_sdk::{contractevent, contracttype, Address, Env, Vec};

/// Minimum shares per exit, except a full-balance redeem/withdraw of dust left over.
/// Blocks 1-unit spam that would fill the queue cheaply.
pub const MIN_WITHDRAWAL_SHARES: i128 = 1_000;

/// ~7 days of ledgers (5s). Node TTL matches vault balances / cost (`max_ttl`).
const WEEK_OF_LEDGERS: u32 = 120_960;

/// Vault hooks the queue needs beyond [`FeeHost`].
pub trait QueueHost: FeeHost {
    /// Operator allowance or owner auth for `shares`.
    fn authorize_exit(e: &Env, owner: &Address, operator: &Address, shares: i128);
    /// Move `shares` from owner onto the vault contract (escrow).
    fn escrow_shares(e: &Env, owner: &Address, shares: i128);
    /// Return escrowed `shares` from the vault to `owner` (cancel).
    fn release_escrow_shares(e: &Env, owner: &Address, shares: i128);
    /// Idle + liquidity-adapter [`liquid_assets`](crate::adapter::YieldAdapter::liquid_assets).
    fn available_liquidity(e: &Env) -> i128;
    /// Ensure idle, burn escrowed shares, pay `sale.paid`, fee settle + vault Withdraw event.
    fn settle_escrowed_exit(
        e: &Env,
        owner: &Address,
        receiver: &Address,
        shares: i128,
        sale: &Sale,
    );
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawRequest {
    pub id: u64,
    pub owner: Address,
    pub receiver: Address,
    pub shares: i128,
    /// Cost basis detached at enqueue ([`FeeLib::detach_cost`]).
    pub cost: i128,
    /// If set (`withdraw`), settle pays at most this many assets when `sale.paid` is higher.
    pub target_assets: Option<i128>,
}

#[contracttype]
#[derive(Clone)]
struct QueueNode {
    req: WithdrawRequest,
    next: Option<u64>,
}

#[contracttype]
#[derive(Clone)]
enum QueueKey {
    /// FIFO head request id.
    Head,
    /// FIFO tail request id (for O(1) append).
    Tail,
    /// Number of pending requests.
    Len,
    NextId,
    /// Persistent node for request `id`.
    Node(u64),
    /// Optional address allowed to cancel any queued request (queue-layer only).
    Manager,
}

#[contractevent(topics = ["queue", "enqueued"], data_format = "vec")]
pub struct WithdrawEnqueued {
    pub id: u64,
    pub owner: Address,
    pub receiver: Address,
    pub shares: i128,
    pub cost: i128,
    pub target_assets: Option<i128>,
}

#[contractevent(topics = ["queue", "settled"], data_format = "vec")]
pub struct WithdrawSettled {
    pub id: u64,
    pub owner: Address,
    pub receiver: Address,
    pub shares: i128,
    pub paid: i128,
}

#[contractevent(topics = ["queue", "cancelled"], data_format = "vec")]
pub struct WithdrawCancelled {
    pub id: u64,
    pub owner: Address,
    pub shares: i128,
    pub cost: i128,
}

pub fn init(env: &Env) {
    let s = env.storage().instance();
    if !s.has(&QueueKey::Len) {
        s.set(&QueueKey::Len, &0u32);
        s.set(&QueueKey::NextId, &0u64);
        // Head / Tail absent ⇒ empty.
    }
}

pub fn len(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&QueueKey::Len)
        .unwrap_or(0)
}

pub fn is_empty(env: &Env) -> bool {
    len(env) == 0
}

/// FIFO snapshot for clients (walks the linked list).
pub fn queue(env: &Env) -> Vec<WithdrawRequest> {
    let mut out = Vec::<WithdrawRequest>::new(env);
    let mut cur: Option<u64> = env.storage().instance().get(&QueueKey::Head);
    while let Some(id) = cur {
        let node = load_node(env, id);
        out.push_back(node.req);
        cur = node.next;
    }
    out
}

/// Optional queue manager (can cancel any request). Curator sets via plug.
pub fn manager(env: &Env) -> Option<Address> {
    env.storage().instance().get(&QueueKey::Manager)
}

/// Set or clear the queue manager. Caller (vault) must already have authorized Curator.
pub fn set_manager(env: &Env, who: Option<Address>) {
    match who {
        Some(m) => env.storage().instance().set(&QueueKey::Manager, &m),
        None => {
            env.storage().instance().remove(&QueueKey::Manager);
        }
    }
}

fn load_node(env: &Env, id: u64) -> QueueNode {
    env.storage()
        .persistent()
        .get(&QueueKey::Node(id))
        .expect("queue node")
}

fn store_node(env: &Env, id: u64, node: &QueueNode) {
    let key = QueueKey::Node(id);
    env.storage().persistent().set(&key, node);
    let max = env.storage().max_ttl();
    env.storage()
        .persistent()
        .extend_ttl(&key, max.saturating_sub(WEEK_OF_LEDGERS), max);
}

fn remove_node(env: &Env, id: u64) {
    env.storage().persistent().remove(&QueueKey::Node(id));
}

fn set_len(env: &Env, n: u32) {
    env.storage().instance().set(&QueueKey::Len, &n);
}

fn peek_front(env: &Env) -> Option<WithdrawRequest> {
    let head: Option<u64> = env.storage().instance().get(&QueueKey::Head);
    head.map(|id| load_node(env, id).req)
}

fn enqueue(
    env: &Env,
    owner: Address,
    receiver: Address,
    shares: i128,
    cost: i128,
    target_assets: Option<i128>,
) -> u64 {
    assert!(shares > 0, "shares");
    let mut next_id: u64 = env
        .storage()
        .instance()
        .get(&QueueKey::NextId)
        .unwrap_or(0);
    let id = next_id;
    next_id = next_id.saturating_add(1);
    env.storage().instance().set(&QueueKey::NextId, &next_id);

    let req = WithdrawRequest {
        id,
        owner: owner.clone(),
        receiver: receiver.clone(),
        shares,
        cost,
        target_assets,
    };
    let node = QueueNode { req, next: None };
    store_node(env, id, &node);

    let tail: Option<u64> = env.storage().instance().get(&QueueKey::Tail);
    if let Some(t) = tail {
        let mut prev = load_node(env, t);
        prev.next = Some(id);
        store_node(env, t, &prev);
    } else {
        env.storage().instance().set(&QueueKey::Head, &id);
    }
    env.storage().instance().set(&QueueKey::Tail, &id);
    set_len(env, len(env).saturating_add(1));

    WithdrawEnqueued {
        id,
        owner,
        receiver,
        shares,
        cost,
        target_assets,
    }
    .publish(env);

    id
}

fn pop_front(env: &Env) -> WithdrawRequest {
    let head: u64 = env
        .storage()
        .instance()
        .get(&QueueKey::Head)
        .expect("queue empty");
    let node = load_node(env, head);
    remove_node(env, head);

    match node.next {
        Some(n) => env.storage().instance().set(&QueueKey::Head, &n),
        None => {
            env.storage().instance().remove(&QueueKey::Head);
            env.storage().instance().remove(&QueueKey::Tail);
        }
    }
    set_len(env, len(env).saturating_sub(1));
    node.req
}

fn sale_for_request<H: FeeHost>(env: &Env, req: &WithdrawRequest) -> Sale {
    let mut sale = FeeLib::sale_with_cost::<H>(env, req.shares, req.cost);
    if let Some(target) = req.target_assets {
        if sale.paid >= target {
            sale.penalty += sale.paid - target;
            sale.paid = target;
        }
    }
    sale
}

fn enqueue_exit<H: QueueHost>(
    env: &Env,
    shares: i128,
    target_assets: Option<i128>,
    receiver: &Address,
    owner: &Address,
    operator: &Address,
) -> u64 {
    assert!(shares > 0, "shares");
    let bal = H::balance(env, owner);
    // Spam floor; full-balance exit and dust positions (< MIN) are always allowed.
    assert!(
        shares >= MIN_WITHDRAWAL_SHARES || shares == bal || bal < MIN_WITHDRAWAL_SHARES,
        "min withdrawal shares"
    );
    H::authorize_exit(env, owner, operator, shares);
    let cost = FeeLib::detach_cost::<H>(env, owner, shares);
    H::escrow_shares(env, owner, shares);
    enqueue(
        env,
        owner.clone(),
        receiver.clone(),
        shares,
        cost,
        target_assets,
    )
}

/// Assets-targeted withdrawal. Returns shares escrowed (or burned on fast path).
///
/// If the queue was empty, attempts to settle this request immediately when liquid.
/// If others are already waiting, only enqueues — no backlog flush.
pub fn withdraw_assets<H: QueueHost>(
    env: &Env,
    assets: i128,
    receiver: &Address,
    owner: &Address,
    operator: &Address,
) -> i128 {
    let shares =
        FeeLib::shares_for::<H>(env, Some(owner), assets).expect("exceeded max withdraw");
    let alone = is_empty(env);
    enqueue_exit::<H>(env, shares, Some(assets), receiver, owner, operator);
    if alone {
        let _ = process::<H>(env, None, 1);
    }
    shares
}

/// Shares redeem. Returns assets paid (0 if still queued).
///
/// If the queue was empty, attempts to settle this request immediately when liquid.
/// If others are already waiting, only enqueues — no backlog flush.
pub fn redeem_shares<H: QueueHost>(
    env: &Env,
    shares: i128,
    receiver: &Address,
    owner: &Address,
    operator: &Address,
) -> i128 {
    assert!(
        shares <= H::balance(env, owner),
        "exceeded max redeem"
    );
    let alone = is_empty(env);
    let id = enqueue_exit::<H>(env, shares, None, receiver, owner, operator);
    if alone {
        process::<H>(env, Some(id), 1).1
    } else {
        0
    }
}

/// Request owner or queue [`manager`]: leave the queue, restore cost + escrow to the
/// **request owner** (not necessarily the caller).
pub fn cancel<H: QueueHost>(env: &Env, caller: &Address, id: u64) {
    let mut prev: Option<u64> = None;
    let mut cur: Option<u64> = env.storage().instance().get(&QueueKey::Head);
    let mut found: Option<QueueNode> = None;
    while let Some(cid) = cur {
        let node = load_node(env, cid);
        if cid == id {
            found = Some(node);
            break;
        }
        prev = Some(cid);
        cur = node.next;
    }
    let node = found.expect("not queued");
    let req = node.req;
    let is_owner = req.owner == *caller;
    let is_manager = manager(env).as_ref() == Some(caller);
    assert!(is_owner || is_manager, "not authorized");

    // Unlink.
    match prev {
        None => match node.next {
            Some(n) => env.storage().instance().set(&QueueKey::Head, &n),
            None => {
                env.storage().instance().remove(&QueueKey::Head);
                env.storage().instance().remove(&QueueKey::Tail);
            }
        },
        Some(p) => {
            let mut pnode = load_node(env, p);
            pnode.next = node.next;
            store_node(env, p, &pnode);
            if node.next.is_none() {
                env.storage().instance().set(&QueueKey::Tail, &p);
            }
        }
    }
    remove_node(env, id);
    set_len(env, len(env).saturating_sub(1));

    FeeLib::attach_cost(env, &req.owner, req.cost);
    H::release_escrow_shares(env, &req.owner, req.shares);
    WithdrawCancelled {
        id: req.id,
        owner: req.owner,
        shares: req.shares,
        cost: req.cost,
    }
    .publish(env);
}

/// Settle head requests FIFO until empty, illiquid, or `max` settlements.
/// `max == 0` means no limit. Returns `(settled_count, tracked_paid)`.
pub fn process<H: QueueHost>(env: &Env, track_id: Option<u64>, max: u32) -> (u32, i128) {
    let mut n = 0u32;
    let mut tracked_paid = 0i128;
    loop {
        if max != 0 && n >= max {
            break;
        }
        let Some(head) = peek_front(env) else {
            break;
        };
        let sale = sale_for_request::<H>(env, &head);
        let need = sale
            .paid
            .checked_add(sale.performance)
            .expect("overflow");
        if H::available_liquidity(env) < need {
            break;
        }
        let req = pop_front(env);
        H::settle_escrowed_exit(env, &req.owner, &req.receiver, req.shares, &sale);
        WithdrawSettled {
            id: req.id,
            owner: req.owner.clone(),
            receiver: req.receiver.clone(),
            shares: req.shares,
            paid: sale.paid,
        }
        .publish(env);
        if track_id == Some(req.id) {
            tracked_paid = sale.paid;
        }
        n += 1;
    }
    (n, tracked_paid)
}
