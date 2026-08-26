# PRD — cora-agent

| | |
|---|---|
| **Produk** | cora-agent — durable Rust agent harness |
| **Repo** | `codecoradev/cora-agent` (MIT, adaptasi pi harness spec) |
| **Versi dokumen** | v1.0 (Phase 0) |
| **Status** | Draft — disetujui sebelum Phase 1 coding |
| **Owner** | Anaz (internal dev) |

---

## 1. Problem Statement

Agent LLM saat ini (pi, Claude Code, dll.) bersifat **ephemeral**: state hidup di memori, crash = hilangnya konteks, dan tidak ada audit trail yang bisa di-replay. Untuk workflow internal CodeCora yang butuh **durability, safety, dan auditability** (fix GitHub issue, merespon temuan cora-review, tanya-jawab dokumentasi), harness yang ada tidak memenuhi:

1. **Tidak durable** — crash di tengah turn mengulang kerja dari nol, tool calls mahal diulang.
2. **Tidak ada approval gate** — tool eksekusi langsung; tidak ada tiering ReadOnly/Write/Destructive.
3. **Tidak embeddable** — harness monolitik CLI; Corin (desktop) dan mobile (Flutter) tidak bisa memakai core yang sama.
4. **Tidak terintegrasi ekosistem Cora** — cora search (code graph), uteke (memory), gh CLI tidak first-class.

### Goals

- Harness Rust dengan **write-once conversation tree** (append-only entries, tidak bisa dimutasi) di atas SQLite.
- **Register state machine** — namespaced mutable cells (`lane/op/pending/fact`) sebagai program counter operasi.
- **Risk-tiered approval gates**: ReadOnly / Write / Destructive.
- **Embeddable core**: platform-agnostic library (no stdin/stdout), host = CLI / Corin / flutter_rust_bridge FFI.
- Integrasi Cora ecosystem: cora search tool, uteke memory, gh CLI.

### ⛔ Gate: 3 Internal Workflows wajib didefinisikan SEBELUM Phase 1 coding

| # | Workflow (placeholder) | Deskripsi | Status |
|---|---|---|---|
| W1 | `fix-gh-issue` | Agent baca GitHub issue via gh CLI → cora search untuk lokasi kode → patch → buat PR. | **[DEFINISIKAN SEBELUM PHASE 1]** |
| W2 | `cora-review-responder` | Agent terima findings dari cora review diff → triage severity → draft reply PR comment. | **[DEFINISIKAN SEBELUM PHASE 1]** |
| W3 | `docs-qnA` | QnA dokumentasi internal via uteke recall + cora brain search → jawaban dengan sitasi. | **[DEFINISIKAN SEBELUM PHASE 1]** |

> Setiap workflow harus punya: trigger, langkah tool calls, risk tier maksimal, dan expected outcome tertulis. Tanpa ini, Phase 1 tidak boleh mulai.

---

## 2. Non-Goals (v0)

| Non-goal | Alasan / kapan dievaluasi |
|---|---|
| Multi-lane (parallel lanes) | v0 = **single lane**; lane register disiapkan tapi hanya 1 aktif. Multi-lane pasca-MVP. |
| TUI / interactive UI | Host menyediakan UI; core tidak punya UI assumption. |
| Parallel tool execution | Tool calls sekuensial per turn; simplifikasi state machine. |
| Deferred redemption | Approval harus resolve sinkron di gate; tidak ada antrian redemption. |
| Cross-build CI (macOS/Windows) | Linux-only sampai **Phase 3**; CI lint+test Linux dulu. |

---

## 3. User Personas

| Persona | Kebutuhan |
|---|---|
| **Internal dev (Anaz)** | Menjalankan workflow otomatis via CLI; crash-safe resume; audit log; approval gate untuk operasi berisik. |
| **Embed host: Corin (desktop, Tauri)** | Embed core crate sebagai library; Approver interaktif via UI Corin. |
| **Embed host: Flutter mobile** | FFI via flutter_rust_bridge; API surface minimal & serializable. |

---

## 4. Functional Requirements (per Module)

| Module | ID | Requirement | Prioritas |
|---|---|---|---|
| **entry** | FR-E1 | Entry = node write-once pada conversation tree; sekali ditulis tidak bisa diubah/dihapus (immutable, append-only). | P0 |
| **entry** | FR-E2 | Entry menyimpan role, payload (message/tool-call/tool-result), parent link, UUIDv7. | P0 |
| **register** | FR-R1 | Namespaced mutable cells: `lane`, `op`, `pending`, `fact`; write melalui transaksi SQLite. | P0 |
| **register** | FR-R2 | Register adalah satu-satunya state mutable; berfungsi sebagai program counter operasi. | P0 |
| **state** | FR-S1 | Operation state machine dengan transisi eksplisit (mis. idle → running → awaiting-approval → running → done/failed); transisi invalid ditolak. | P0 |
| **state** | FR-S2 | Setelah crash, state machine + register dipulihkan dari SQLite dan operasi resume dari checkpoint. | P0 |
| **storage** | FR-D1 | SQLite backend, **one file per session**; WAL mode. | P0 |
| **storage** | FR-D2 | Schema versioned (`STORAGE_VERSION`), **migrate-on-open**; versi lebih baru dari binary → error jelas. | P0 |
| **tool** | FR-T1 | `Tool` trait dengan risk tier: `ReadOnly` / `Write` / `Destructive`; metadata (nama, deskripsi, schema argumen) diekspos ke provider. | P0 |
| **tool** | FR-T2 | Tool execution sekuensial; hasil (termasuk error) dicatat sebagai entry tool-result. | P0 |
| **approval** | FR-A1 | `Approver` trait: core menyediakan policy engine; impl `Allowlist` / `Interactive` hidup di host. | P0 |
| **approval** | FR-A2 | Tool call dengan tier ≥ configured threshold wajib lewat approver sebelum eksekusi; keputusan dicatat di tree. | P0 |
| **provider** | FR-P1 | `Provider` trait abstraksi LLM (stream/chat completion); implementasi concrete di host atau crate terpisah. | P0 |
| **provider** | FR-P2 | Provider menerima snapshot tree (atau window) + tool metadata; tidak pernah menulis register langsung. | P0 |

> Catatan: `provider` saat ini belum punya file stub di core (lib.rs mendokumentasikannya) — buat `provider.rs` di Phase 1a.

---

## 5. Risk Tiers & Approval Matrix

| Tier | Definisi | Contoh tool | Approval default |
|---|---|---|---|
| **ReadOnly** | Tidak mengubah state dunia | cora search, uteke recall, gh view | Auto-allow (allowlist) |
| **Write** | Mengubah state, reversible | patch file, gh pr create, uteke remember | Approver wajib (auto-allow untuk allowlist internal) |
| **Destructive** | Tidak reversible / berdampak luas | force push, drop database, delete branch, `rm -rf` | Approver wajib, **selalu interactive** untuk internal dev; tidak bisa di-allowlist |

Matriks keputusan:

| Tool tier \ Approver | Auto-allow | Interactive |
|---|---|---|
| ReadOnly | ✅ eksekusi | ✅ (bisa prompt) |
| Write | ✅ jika ada di allowlist | ✅ prompt |
| Destructive | ❌ ditolak | ✅ prompt + konfirmasi eksplisit |

---

## 6. Roadmap & Definition of Done

| Phase | Scope | DoD (verifiable) |
|---|---|---|
| **0 — Stubs & spec** (selesai) | Crate layout, stubs, spec.md | ✅ 7 file stub + lib.rs; workspace build hijau |
| **1a — Store + State machine + Mock provider** | entry, register, state, storage (SQLite), tool trait, approval trait, mock provider | Unit test lulus: (1) entry append-only enforced, (2) crash di tengah op → restart → resume dari checkpoint (test kill/reopen DB), (3) transisi state invalid ditolak, (4) migrate-on-open berfungsi. Demo CLI: mini-loop dengan mock provider + 1 tool ReadOnly. |
| **1b — cora search tool + real LLM** | cora search tool (ReadOnly), provider real (LLM via API), W1 `fix-gh-issue` workflow | E2E: W1 berjalan penuh di CLI dengan LLM real; tool call cora search tercatat di tree; approval gate Write terpicu saat patch. |
| **2 — Workflow 2 & 3** | W2 cora-review-responder, W3 docs-QnA (uteke), gh CLI tools, allowlist policy | E2E: W2 & W3 selesai penuh; audit replay (baca tree → timeline) tersedia. |
| **3 — Hardening & embed prep** | Cross-build CI (macOS/Windows), FFI surface, dokumentasi public API, publikasi crate | CI matrix hijau; contoh embed minimal (Corin spike atau FFI test) compile; docs API lengkap. |

### Roadmap positioning (strategic)

```
Internal tool (Phase 0-2) → OSS core release post-MVP (Phase 3) → Corin embed (CMO) → Flutter mobile
```

Core selalu library-first; CLI hanyalah host pertama. Public API dibekukan/dikunci semantik saat rilis OSS.

---

## 7. Kill Criteria (dari CFO)

Proyek dihentikan/di-frozen jika terpenuhi ≥2:

1. **Crash-resume tidak bisa dibuktikan bekerja** setelah Phase 1a + 1 iterasi perbaikan.
2. **3 internal workflows tidak terpakai** — tidak ada usage nyata >1 bulan setelah Phase 2.
3. **Maintenance >20% waktu hemat** yang seharusnya dihasilkan (negative ROI berkelanjutan).
4. **Alternatif eksternal memenuhi kebutuhan** (harness OSS existing cukup durable + embeddable + integrasi Cora bisa dibuat sebagai plugin).
5. **Biaya LLM per workflow tidak masuk akal** dibanding manual (tidak ada jalur optimasi).
6. **Blocker teknis struktural** — SQLite state machine terbukti tidak cukup untuk complexity workflow nyata (bukan sekadar bug).

---

## 8. Success Metrics

| Metric | Target | Cara ukur |
|---|---|---|
| Crash-resume | Test deterministik kill/restart lulus; ≥1 insiden nyata resume sukses | Test suite + log session |
| Internal workflows E2E | **>2 workflow** (dari W1–W3) selesai end-to-end tanpa intervensi manusia di luar approval | Audit tree per session |
| Maintenance overhead | **<20%** dari waktu yang dihemat oleh agent | Time-tracking kasar per bulan |
| Approval compliance | 0 eksekusi Destructive tanpa approval tercatat | Audit tree scan |
| Embeddability | Core crate compile tanpa feature CLI/UI; API serializable | CI check Phase 3 |

---

## 9. Referensi

- `spec.md` (pi harness spec, MIT) — dasar desain entry/register/state.
- `crates/cora-agent-core/src/lib.rs` — layout modul & terminologi resmi.
- Cora ecosystem: `cora` (code graph & review), `uteke` (semantic memory), `gh` CLI.
