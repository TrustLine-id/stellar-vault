// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Trustline Digital Asset Ltd.
//! Curator timelock: per-op delays, default at deploy, adjustable.
//!
//! Flow: Curator `submit` (Trustline) → wait op-specific delay → anyone executes target.
//! `DecreaseTimelock` submit waits the **target** op's current delay.
//!
//! Meta ops: `IncreaseTimelock`, `DecreaseTimelock`, `Abdicate` (irreversible).

use soroban_sdk::{contracterror, contractevent, contracttype, panic_with_error, Address, Env, Vec};

/// Max curator timelock.
pub const MAX_TIMELOCK: u64 = 365 * 86_400;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    NothingPending = 1,
    NotReady = 2,
    NotSubmitted = 3,
    Abdicated = 4,
    InvalidDuration = 5,
    AlreadyAbdicated = 6,
}

/// Timelock bucket (one delay per operation kind).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimelockOpKind {
    AddAdapter,
    RemoveAdapter,
    SetIsAllocator,
    IncreaseTimelock,
    DecreaseTimelock,
    Abdicate,
    IncreaseAbsoluteCap,
    IncreaseRelativeCap,
    SetFees,
}

/// Sensitive curator operation queued for later execution.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PendingOp {
    AddAdapter(Address),
    RemoveAdapter(Address),
    SetIsAllocator(Address, bool),
    IncreaseTimelock(TimelockOpKind, u64),
    DecreaseTimelock(TimelockOpKind, u64),
    Abdicate(TimelockOpKind),
    IncreaseAbsoluteCap(Address, i128),
    IncreaseRelativeCap(Address, i128),
    /// Flat fee params (changed via this general timelock).
    SetFees(Address, u32, u32, Vec<(i128, u32)>),
}

#[contracttype]
#[derive(Clone)]
pub enum TimelockKey {
    DefaultDelay,
    OpDelay(TimelockOpKind),
    Abdicated(TimelockOpKind),
    Pending(PendingOp),
    Sentinels,
    IsSentinel(Address),
}

#[contractevent(topics = ["timelock", "submitted"], data_format = "vec")]
pub struct TimelockSubmitted {
    pub op: PendingOp,
    pub eta: u64,
}

#[contractevent(topics = ["timelock", "revoked"], data_format = "vec")]
pub struct TimelockRevoked {
    pub op: PendingOp,
}

#[contractevent(topics = ["timelock", "increased"], data_format = "vec")]
pub struct TimelockIncreased {
    pub kind: TimelockOpKind,
    pub new_duration: u64,
}

#[contractevent(topics = ["timelock", "decreased"], data_format = "vec")]
pub struct TimelockDecreased {
    pub kind: TimelockOpKind,
    pub new_duration: u64,
}

#[contractevent(topics = ["timelock", "abdicated"], data_format = "vec")]
pub struct TimelockAbdicated {
    pub kind: TimelockOpKind,
}

const ALL_KINDS: [TimelockOpKind; 9] = [
    TimelockOpKind::AddAdapter,
    TimelockOpKind::RemoveAdapter,
    TimelockOpKind::SetIsAllocator,
    TimelockOpKind::IncreaseTimelock,
    TimelockOpKind::DecreaseTimelock,
    TimelockOpKind::Abdicate,
    TimelockOpKind::IncreaseAbsoluteCap,
    TimelockOpKind::IncreaseRelativeCap,
    TimelockOpKind::SetFees,
];

pub fn init(env: &Env, default_delay: u64) {
    assert!(default_delay <= MAX_TIMELOCK, "timelock");
    env.storage()
        .instance()
        .set(&TimelockKey::DefaultDelay, &default_delay);
    for kind in ALL_KINDS.iter() {
        env.storage()
            .instance()
            .set(&TimelockKey::OpDelay(kind.clone()), &default_delay);
    }
    env.storage()
        .instance()
        .set(&TimelockKey::Sentinels, &Vec::<Address>::new(env));
}

pub fn default_delay(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&TimelockKey::DefaultDelay)
        .unwrap()
}

pub fn op_delay(env: &Env, kind: &TimelockOpKind) -> u64 {
    env.storage()
        .instance()
        .get(&TimelockKey::OpDelay(kind.clone()))
        .unwrap_or_else(|| default_delay(env))
}

pub fn is_abdicated(env: &Env, kind: &TimelockOpKind) -> bool {
    env.storage()
        .instance()
        .get(&TimelockKey::Abdicated(kind.clone()))
        .unwrap_or(false)
}

pub fn executable_at(env: &Env, op: &PendingOp) -> Option<u64> {
    env.storage().instance().get(&TimelockKey::Pending(op.clone()))
}

pub fn is_sentinel(env: &Env, who: &Address) -> bool {
    env.storage()
        .instance()
        .get(&TimelockKey::IsSentinel(who.clone()))
        .unwrap_or(false)
}

pub fn sentinels(env: &Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&TimelockKey::Sentinels)
        .unwrap_or_else(|| Vec::new(env))
}

pub fn set_is_sentinel(env: &Env, who: Address, enabled: bool) {
    let currently = is_sentinel(env, &who);
    if currently == enabled {
        return;
    }
    env.storage()
        .instance()
        .set(&TimelockKey::IsSentinel(who.clone()), &enabled);
    let list = sentinels(env);
    if enabled {
        let mut next = list;
        next.push_back(who);
        env.storage().instance().set(&TimelockKey::Sentinels, &next);
    } else {
        let mut next = Vec::<Address>::new(env);
        for i in 0..list.len() {
            let a = list.get(i).unwrap();
            if a != who {
                next.push_back(a);
            }
        }
        env.storage().instance().set(&TimelockKey::Sentinels, &next);
    }
}

/// Which timelock bucket governs the wait before this pending op can execute.
pub fn submit_wait(env: &Env, op: &PendingOp) -> u64 {
    match op {
        PendingOp::DecreaseTimelock(kind, _) => op_delay(env, kind),
        _ => op_delay(env, &execute_kind(op)),
    }
}

/// Timelock bucket checked for abdication before submit / execute.
pub fn execute_kind(op: &PendingOp) -> TimelockOpKind {
    match op {
        PendingOp::AddAdapter(_) => TimelockOpKind::AddAdapter,
        PendingOp::RemoveAdapter(_) => TimelockOpKind::RemoveAdapter,
        PendingOp::SetIsAllocator(_, _) => TimelockOpKind::SetIsAllocator,
        PendingOp::IncreaseTimelock(_, _) => TimelockOpKind::IncreaseTimelock,
        PendingOp::DecreaseTimelock(_, _) => TimelockOpKind::DecreaseTimelock,
        PendingOp::Abdicate(_) => TimelockOpKind::Abdicate,
        PendingOp::IncreaseAbsoluteCap(_, _) => TimelockOpKind::IncreaseAbsoluteCap,
        PendingOp::IncreaseRelativeCap(_, _) => TimelockOpKind::IncreaseRelativeCap,
        PendingOp::SetFees(_, _, _, _) => TimelockOpKind::SetFees,
    }
}

fn assert_not_abdicated(env: &Env, kind: &TimelockOpKind) {
    if is_abdicated(env, kind) {
        panic_with_error!(env, TimelockError::Abdicated);
    }
}

fn check_duration(env: &Env, duration: u64) {
    if duration > MAX_TIMELOCK {
        panic_with_error!(env, TimelockError::InvalidDuration);
    }
}

/// Curator queues `op` (Trustline gated at the vault entrypoint).
pub fn submit(env: &Env, op: PendingOp) {
    assert_not_abdicated(env, &execute_kind(&op));
    let eta = env.ledger().timestamp() + submit_wait(env, &op);
    env.storage()
        .instance()
        .set(&TimelockKey::Pending(op.clone()), &eta);
    TimelockSubmitted { op, eta }.publish(env);
}

pub fn revoke(env: &Env, op: PendingOp) {
    let key = TimelockKey::Pending(op.clone());
    if !env.storage().instance().has(&key) {
        panic_with_error!(env, TimelockError::NothingPending);
    }
    env.storage().instance().remove(&key);
    TimelockRevoked { op }.publish(env);
}

/// Consume a due pending op. Always requires a prior `submit`.
pub fn begin_execute(env: &Env, op: &PendingOp) {
    assert_not_abdicated(env, &execute_kind(op));
    let Some(eta) = env
        .storage()
        .instance()
        .get::<_, u64>(&TimelockKey::Pending(op.clone()))
    else {
        panic_with_error!(env, TimelockError::NotSubmitted);
    };
    if env.ledger().timestamp() < eta {
        panic_with_error!(env, TimelockError::NotReady);
    }
    env.storage()
        .instance()
        .remove(&TimelockKey::Pending(op.clone()));
}

pub fn apply_increase(env: &Env, kind: TimelockOpKind, new_duration: u64) {
    check_duration(env, new_duration);
    let current = op_delay(env, &kind);
    if new_duration <= current {
        panic_with_error!(env, TimelockError::InvalidDuration);
    }
    env.storage()
        .instance()
        .set(&TimelockKey::OpDelay(kind.clone()), &new_duration);
    TimelockIncreased {
        kind,
        new_duration,
    }
    .publish(env);
}

pub fn apply_decrease(env: &Env, kind: TimelockOpKind, new_duration: u64) {
    check_duration(env, new_duration);
    let current = op_delay(env, &kind);
    if new_duration >= current {
        panic_with_error!(env, TimelockError::InvalidDuration);
    }
    env.storage()
        .instance()
        .set(&TimelockKey::OpDelay(kind.clone()), &new_duration);
    TimelockDecreased {
        kind,
        new_duration,
    }
    .publish(env);
}

pub fn apply_abdicate(env: &Env, kind: TimelockOpKind) {
    if is_abdicated(env, &kind) {
        panic_with_error!(env, TimelockError::AlreadyAbdicated);
    }
    env.storage()
        .instance()
        .set(&TimelockKey::Abdicated(kind.clone()), &true);
    TimelockAbdicated { kind }.publish(env);
}
