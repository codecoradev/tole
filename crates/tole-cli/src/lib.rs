//! Host-side pieces of the CLI exposed as a library, so integration
//! tests exercise the REAL implementations — the approval flow and the
//! jailed file tools — instead of test-local re-implementations that
//! drift from production behavior (CodeCora scan 2026-09-18).
pub mod approver;
pub mod tools;
