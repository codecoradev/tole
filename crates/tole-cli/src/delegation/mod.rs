//! Delegation host side: the `delegate` tool and its `decide` flow, the
//! delegated `approver` and `approvals_owner` commands, the `audit` log, the
//! `machine_guard`, the `ledger_file` locator and `serve_delegation`.

pub mod approvals_owner;
pub mod approver;
pub mod audit;
pub mod decide;
pub mod delegate;
pub mod ledger_file;
pub mod machine_guard;
pub mod serve_delegation;
