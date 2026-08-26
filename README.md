# cora-agent

Durable Rust agent harness: write-once conversation tree + register state machine over SQLite, with risk-tiered approval gates.

Status: Phase 0 - blueprint and scaffold.

## Workspace
- crates/cora-agent-core - platform-agnostic lib (state machine, storage, Tool/Approver traits)
- crates/cora-agent-cli - headless CLI host

## Design sources
Internal research doc tree: cora-agent-research (uteke). Core concepts adapted from pi's harness spec (MIT).
