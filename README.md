# tole

Durable Rust agent harness: write-once conversation tree + register state machine over JSONL session files, with risk-tiered approval gates.

Status: Phase 0 - blueprint and scaffold.

## Workspace
- crates/tole-core - platform-agnostic lib (state machine, storage, Tool/Approver traits)
- crates/tole-cli - headless CLI host

## Design sources
Research notes and design sources: adapted from pi's harness spec (MIT).
