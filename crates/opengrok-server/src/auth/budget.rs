//! Budgets for the doors anybody on the network can call in a loop.
//!
//! Password-reset and verification mail, dynamic client registration, the credential forms and a domain lookup
//! take no credential (or take one they are about to check), so nothing else bounds what they
//! cost: a mail per call, a row per call, a hash per call, a DNS query per call. Each gets a
//! budget — so many hits per key per window — and once it is spent the door answers 429 with a
//! `Retry-After`, in plain words. One door here does take a credential: a coworker-less `/ag-ui`
//! turn, which has a payer but nothing that meters it (`AGUI_UNSCOPED`).
//!
//! In memory, per replica, on purpose. A limit exists to bound cost, and a fleet of N replicas
//! each letting through its own budget still bounds it (at N times the figure); a table would
//! turn every unauthenticated request into the database write the limit is there to prevent.
//! What DOES have to be shared across replicas (logins, codes, one-shot approvals) lives in the
//! store, not here.
//!
//! A poisoned lock counts as spent: a limiter that fails open is not one (CLAUDE.md #8).

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

const HOUR_MS: i64 = 60 * 60 * 1_000;
/// Past this many keys the whole table is pruned of spent-and-expired entries on the next hit,
/// so an attacker rotating addresses cannot grow the map without bound.
const PRUNE_ABOVE: usize = 4_096;

/// One door's allowance: `per_window` hits per key inside a sliding `window_ms`.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Names the door in the table's key so two doors never share a bucket.
    pub name: &'static str,
    pub per_window: usize,
    pub window_ms: i64,
}

/// `POST /auth/password/forgot` (both bodies): a mail per call. Per address AND per target
/// email — the second stops one address being mail-bombed from many peers, at the price that
/// somebody can hold a reset off for an hour by spending its budget. The reply stays the same
/// either way, so the budget discloses nothing about whether the address has an account.
pub const FORGOT: Budget = Budget {
    name: "forgot",
    per_window: 5,
    window_ms: HOUR_MS,
};
/// `POST /auth/verify/resend` (both bodies): a mail per call, budgeted exactly like `FORGOT` and
/// for the same reasons — per address and per mailbox, with a reply that never changes.
pub const VERIFY_RESEND: Budget = Budget {
    name: "verify-resend",
    per_window: 5,
    window_ms: HOUR_MS,
};
/// `POST /admin/domains/{domain}/verify`, per org: a resolver round trip per call.
pub const DOMAIN_VERIFY: Budget = Budget {
    name: "domain-verify",
    per_window: 12,
    window_ms: HOUR_MS,
};
/// `POST /oauth/mcp/register`, per address: unauthenticated by design (RFC 7591), a row per call.
pub const CLIENT_REGISTRATION: Budget = Budget {
    name: "client-registration",
    per_window: 20,
    window_ms: HOUR_MS,
};
/// Wrong credentials on `/auth/login` and `/loginDeepControl`, per address. Only FAILURES count:
/// a household behind one NAT signing in all day is not guessing, and the smokes sign in dozens
/// of times from one loopback address.
pub const LOGIN_FAILURES: Budget = Budget {
    name: "login-failures",
    per_window: 30,
    window_ms: HOUR_MS,
};

/// `POST /ag-ui` turns that name no coworker, per account. Such a turn has a payer but no gateway
/// key of its own, so it runs on the deployment's key where no meter sees it (`GuardedDoor` meters
/// a coworker's key); this is the bound on what one account can spend that way. A coworker's
/// turn is metered and capped by its own key instead, and never counts here.
pub const AGUI_UNSCOPED: Budget = Budget {
    name: "agui-unscoped",
    per_window: 60,
    window_ms: HOUR_MS,
};

/// A budget that is spent for this key; how long until one hit frees up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spent {
    pub retry_after_secs: u64,
}

/// The hit table, one per replica, shared by every limited door.
#[derive(Debug, Default)]
pub struct Budgets {
    hits: Mutex<HashMap<(&'static str, String), Vec<i64>>>,
}

impl Budgets {
    /// Spend one hit of `budget` for `key` now, or say how long until one is available.
    pub fn take(&self, budget: &Budget, key: &str) -> Result<(), Spent> {
        self.take_at(budget, key, now_ms())
    }

    /// Is a hit available, without spending it. For the doors that only count failures.
    pub fn check(&self, budget: &Budget, key: &str) -> Result<(), Spent> {
        self.check_at(budget, key, now_ms())
    }

    /// Record a hit without asking — the failure just happened.
    pub fn hit(&self, budget: &Budget, key: &str) {
        self.hit_at(budget, key, now_ms());
    }

    /// The clock is a parameter so a test can watch a window slide without sleeping through it.
    pub fn take_at(&self, budget: &Budget, key: &str, now_ms: i64) -> Result<(), Spent> {
        let Ok(mut hits) = self.hits.lock() else {
            return Err(Spent {
                retry_after_secs: to_secs(budget.window_ms),
            });
        };
        if hits.len() > PRUNE_ABOVE {
            hits.retain(|(_, _), list| list.iter().any(|t| now_ms - *t < HOUR_MS));
        }
        let list = hits.entry((budget.name, key.to_string())).or_default();
        list.retain(|t| now_ms - *t < budget.window_ms);
        match spent(budget, list, now_ms) {
            Some(spent) => Err(spent),
            None => {
                list.push(now_ms);
                Ok(())
            }
        }
    }

    pub fn check_at(&self, budget: &Budget, key: &str, now_ms: i64) -> Result<(), Spent> {
        let Ok(hits) = self.hits.lock() else {
            return Err(Spent {
                retry_after_secs: to_secs(budget.window_ms),
            });
        };
        match hits.get(&(budget.name, key.to_string())) {
            Some(list) => {
                let live: Vec<i64> = list
                    .iter()
                    .copied()
                    .filter(|t| now_ms - *t < budget.window_ms)
                    .collect();
                spent(budget, &live, now_ms).map_or(Ok(()), Err)
            }
            None => Ok(()),
        }
    }

    /// Give back the newest hit for `key`: a charge taken up front for something that turned out
    /// not to happen (a retried POST that reattaches to a run already going). A poisoned lock
    /// keeps the charge — the fail-closed side.
    pub fn refund(&self, budget: &Budget, key: &str) {
        if let Ok(mut hits) = self.hits.lock()
            && let Some(list) = hits.get_mut(&(budget.name, key.to_string()))
            && let Some(newest) = list
                .iter()
                .enumerate()
                .max_by_key(|(_, at_ms)| **at_ms)
                .map(|(index, _)| index)
        {
            list.swap_remove(newest);
        }
    }

    pub fn hit_at(&self, budget: &Budget, key: &str, now_ms: i64) {
        if let Ok(mut hits) = self.hits.lock() {
            let list = hits.entry((budget.name, key.to_string())).or_default();
            list.retain(|t| now_ms - *t < budget.window_ms);
            list.push(now_ms);
        }
    }
}

/// `Some` when the live hits fill the budget: the wait is until the oldest one leaves the window.
fn spent(budget: &Budget, live: &[i64], now_ms: i64) -> Option<Spent> {
    if live.len() < budget.per_window {
        return None;
    }
    let oldest = live.iter().copied().min().unwrap_or(now_ms);
    Some(Spent {
        retry_after_secs: to_secs(oldest + budget.window_ms - now_ms),
    })
}

/// Rounded up, never zero: a `Retry-After: 0` reads as "try again now", which is the one thing
/// a spent budget must not say.
fn to_secs(ms: i64) -> u64 {
    let ms = ms.max(1);
    u64::try_from((ms + 999) / 1_000).unwrap_or(1)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The address a hit is charged to. The HTTPS front (Caddy, `docs/setup/tls.md`) sets
/// `X-Forwarded-For`; a bare deployment without one charges everybody to `unknown`, which is a
/// shared budget rather than none — the fail-closed side of not knowing.
pub fn peer_of(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// An email as a budget key: the same mailbox however it was typed.
pub fn email_key(email: &str) -> String {
    format!("email:{}", email.trim().to_ascii_lowercase())
}

pub fn peer_key(headers: &HeaderMap) -> String {
    format!("peer:{}", peer_of(headers))
}

/// Stamp `Retry-After` on any refusal — the page, the JSON, the OAuth error — so a client that
/// reads it waits the right amount and one that does not still sees a 429.
pub fn with_retry_after(mut response: Response, spent: Spent) -> Response {
    if let Ok(value) = spent.retry_after_secs.to_string().parse() {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The JSON refusal: `429`, a sentence, and the seconds again in the body for clients that
/// cannot read headers (a browser page's `fetch` can, but the console shows the number).
pub fn too_many(spent: Spent, message: &str) -> Response {
    with_retry_after(
        (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "error": message,
                "retryAfterSecs": spent.retry_after_secs,
            })),
        )
            .into_response(),
        spent,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/auth_budget.rs"]
mod tests;
