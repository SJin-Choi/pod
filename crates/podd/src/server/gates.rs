//! P5 backend: gates, i.e. pending human approvals. Created by the UI/API or
//! by an agent from inside its terminal (`pod gate request`), announced on
//! every `status.subscribe` stream as `gate.changed`, resolved by the human
//! or (v6, `expires_in_s`) by the expiry worker as `expired`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use super::status::{self, HubEvent};
use super::{Daemon, log};
use crate::protocol::*;

const MAX_MESSAGE: usize = 16 * 1024;
/// Longest accepted `expires_in_s` (one year).
const MAX_EXPIRY_S: u64 = 365 * 24 * 3600;
/// The expiry worker re-checks at least this often.
const EXPIRY_POLL: Duration = Duration::from_secs(5);

fn gate_not_found(id: &str) -> RpcError {
    RpcError::new(orch_codes::GATE_NOT_FOUND, format!("no gate {id:?}"))
}

impl Daemon {
    pub(super) fn gate_create(&self, p: GateCreateParams) -> Result<GateInfo, RpcError> {
        if p.message.trim().is_empty() {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                "message must not be empty",
            ));
        }
        if p.message.len() > MAX_MESSAGE {
            return Err(RpcError::new(codes::INVALID_PARAMS, "message too long"));
        }
        if p.expires_in_s.is_some_and(|s| s == 0 || s > MAX_EXPIRY_S) {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("expires_in_s must be 1..={MAX_EXPIRY_S}"),
            ));
        }
        let terminal = match &p.terminal {
            Some(t) => Some(self.resolve(&TerminalRef::parse(t))?.id.clone()),
            None => None,
        };
        let mut task = match &p.task {
            Some(t) => Some(self.task_record(t)?.id),
            None => None,
        };
        if task.is_none()
            && let Some(tid) = &terminal
        {
            task = self
                .registry
                .lock()
                .unwrap()
                .tasks
                .iter()
                .find(|t| t.terminals.contains(tid))
                .map(|t| t.id.clone());
        }
        let group = match &p.group {
            Some(g) => Some(self.orch.lock().unwrap().group(g)?.id.clone()),
            None => None,
        };
        let gate = {
            let mut st = self.orch.lock().unwrap();
            let now = crate::now_ms();
            let g = GateInfo {
                id: st.next_id("g"),
                title: p
                    .title
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "Approval needed".into()),
                message: p.message,
                task,
                group,
                terminal,
                state: GateState::Pending,
                created_ms: now,
                resolved_ms: None,
                note: None,
                expires_ms: p.expires_in_s.map(|s| now + s * 1000),
            };
            st.gates.push(g.clone());
            self.save_orch(&mut st)?;
            g
        };
        log(&format!(
            "gate {} requested (task {}, terminal {})",
            gate.id,
            gate.task.as_deref().unwrap_or("-"),
            gate.terminal.as_deref().unwrap_or("-")
        ));
        status::publish_gate(gate.clone());
        if gate.expires_ms.is_some() {
            self.gate_wake.notify_one();
        }
        Ok(gate)
    }

    /// Resolve every pending gate whose expiry has passed as `expired`
    /// (persisted, announced with `gate.changed`). Returns the next expiry.
    pub(super) fn expire_gates(&self, now: u64) -> Option<u64> {
        let (expired, next) = {
            let mut st = self.orch.lock().unwrap();
            let mut expired = Vec::new();
            let mut next: Option<u64> = None;
            for g in st
                .gates
                .iter_mut()
                .filter(|g| g.state == GateState::Pending)
            {
                let Some(at) = g.expires_ms else { continue };
                if at <= now {
                    g.state = GateState::Expired;
                    g.resolved_ms = Some(now);
                    g.note = Some(format!(
                        "expired after {} s without a decision",
                        at.saturating_sub(g.created_ms) / 1000
                    ));
                    expired.push(g.clone());
                } else {
                    next = Some(next.map_or(at, |n| n.min(at)));
                }
            }
            if !expired.is_empty()
                && let Err(e) = self.save_orch(&mut st)
            {
                log(&e.message);
            }
            (expired, next)
        };
        for g in expired {
            log(&format!("gate {} expired", g.id));
            status::publish_gate(g);
        }
        next
    }

    pub(super) fn gate_resolve(&self, p: GateResolveParams) -> Result<GateInfo, RpcError> {
        let gate = {
            let mut st = self.orch.lock().unwrap();
            let g = st
                .gates
                .iter_mut()
                .find(|g| g.id == p.id)
                .ok_or_else(|| gate_not_found(&p.id))?;
            if g.state != GateState::Pending {
                let mut e = RpcError::new(
                    orch_codes::GATE_RESOLVED,
                    format!("gate {} is already {:?}", g.id, g.state).to_lowercase(),
                );
                e.data = serde_json::to_value(&*g).ok();
                return Err(e);
            }
            g.state = match p.decision {
                GateDecision::Approve => GateState::Approved,
                GateDecision::Reject => GateState::Rejected,
            };
            g.resolved_ms = Some(crate::now_ms());
            g.note = p.note;
            let g = g.clone();
            self.save_orch(&mut st)?;
            g
        };
        log(&format!("gate {} {:?}", gate.id, gate.state));
        status::publish_gate(gate.clone());
        Ok(gate)
    }

    pub(super) fn gate_list(&self, p: GateListParams) -> Result<GateListResult, RpcError> {
        let task = p.task.as_ref().map(|t| {
            self.task_record(t)
                .map(|r| r.id)
                .unwrap_or_else(|_| t.clone())
        });
        let st = self.orch.lock().unwrap();
        let group = p.group.as_ref().map(|g| {
            st.group(g)
                .map(|r| r.id.clone())
                .unwrap_or_else(|_| g.clone())
        });
        Ok(GateListResult {
            gates: st
                .gates
                .iter()
                .filter(|g| task.is_none() || g.task == task)
                .filter(|g| group.is_none() || g.group == group)
                .filter(|g| p.state.is_none_or(|s| g.state == s))
                .cloned()
                .collect(),
        })
    }

    pub(super) fn gate_show(&self, p: GateRef) -> Result<GateInfo, RpcError> {
        self.orch
            .lock()
            .unwrap()
            .gates
            .iter()
            .find(|g| g.id == p.id)
            .cloned()
            .ok_or_else(|| gate_not_found(&p.id))
    }

    pub(super) async fn gate_wait(&self, p: GateWaitParams) -> Result<GateWaitResult, RpcError> {
        // Subscribe before the first check so no resolution is missed.
        let mut rx = status::subscribe();
        let start = Instant::now();
        let deadline = p
            .timeout_ms
            .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
        loop {
            let g = self.gate_show(GateRef { id: p.id.clone() })?;
            if g.state != GateState::Pending {
                return Ok(GateWaitResult {
                    outcome: WaitOutcome::Matched,
                    gate: g,
                    waited_ms: start.elapsed().as_millis() as u64,
                });
            }
            let next = async {
                loop {
                    match rx.recv().await {
                        Ok(HubEvent::Gate(ev)) if ev.gate.id == p.id => return,
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => return,
                        Err(broadcast::error::RecvError::Closed) => {
                            std::future::pending::<()>().await
                        }
                    }
                }
            };
            let timed_out = match deadline {
                Some(d) => tokio::time::timeout_at(d, next).await.is_err(),
                None => {
                    next.await;
                    false
                }
            };
            if timed_out {
                return Ok(GateWaitResult {
                    outcome: WaitOutcome::TimedOut,
                    gate: g,
                    waited_ms: start.elapsed().as_millis() as u64,
                });
            }
        }
    }
}

/// Start the gate expiry worker: wakes at the next expiry, when a gate with
/// an expiry is created, and every few seconds. Gates that expired while the
/// daemon was down are resolved on the first pass.
pub(super) fn start(daemon: &Arc<Daemon>) {
    let daemon = daemon.clone();
    tokio::spawn(async move {
        loop {
            let next = daemon.expire_gates(crate::now_ms());
            let wait = next
                .map(|at| Duration::from_millis(at.saturating_sub(crate::now_ms())))
                .unwrap_or(EXPIRY_POLL)
                .clamp(Duration::from_millis(20), EXPIRY_POLL);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = daemon.gate_wake.notified() => {}
            }
        }
    });
}
