# cora-agent — Epics & Sprint Plan (6 Minggu)

> Solo dev part-time, kapasitas ~10 jam/minggu. Bahasa Indonesia, istilah teknis English.
> Ground truth roadmap: Phase 0 DONE (scaffold + CI). Phase 1a → 1b → 2 → 3.

## Non-Goals (diulang dari roadmap)

- **Bukan** multi-agent orchestration / planner hierarkis.
- **Bukan** sandbox container execution (Docker/gVisor) — tools berjalan via approval gates.
- **Bukan** UI web/dashboard — CLI only.
- **Bukan** provider non-LLM-first (no rule-based planner).
- **Bukan** distribusi binary selain cross-build CI target yang disebut (aarch64-android, aarch64-apple-ios) — bukan packaging store (Play Store/App Store).

---

## E1 — Storage Layer (SQLite 3-Store Schema + Migrate-on-Open)

- **Phase:** 1a
- **User Stories:**
  - US1.1: Sebagai developer, saya ingin 3 store SQLite (sessions / turns+events / state snapshot) dengan schema `STORAGE_VERSION`-ed, agar evolusi schema terkontrol.
  - US1.2: Sebagai developer, saya ingin migrate-on-open (migrasi idempoten saat DB dibuka), agar upgrade binary tidak merusak data lama.
- **Acceptance Criteria:**
  - [ ] 3 store terpisah dengan schema terdokumentasi di `docs/` atau rustdoc.
  - [ ] Open DB versi N-1 → auto-migrate ke N tanpa data loss (test roundtrip).
  - [ ] Open DB versi > current → hard error dengan pesan jelas (test).
  - [ ] Semua operasi lewat satu `Storage` trait; no raw SQL di caller.
- **Est effort:** M (~12 jam)
- **Dependencies:** — (Phase 0 scaffold).

## E2 — State Machine: Program Counter, Seq CAS, Effect Sandwich

- **Phase:** 1a
- **User Stories:**
  - US2.1: Sebagai developer, saya ingin program counter + sequence number dengan CAS (compare-and-swap), agar concurrency/double-apply effect terdeteksi.
  - US2.2: Sebagai developer, saya ingin pola effect sandwich (persist intent → execute effect → persist result), agar crash di tengah effect dapat di-resume deterministik.
- **Acceptance Criteria:**
  - [ ] `seq` bertambah monoton; stale write (seq mismatch) ditolak dengan error, bukan silent overwrite (unit test).
  - [ ] Test: crash disimulasikan antara "persist intent" dan "execute" → resume menghasilkan state akhir identik dengan run tanpa crash (deterministic replay test).
  - [ ] Program counter memvalidasi transisi step illegal → error, bukan panic.
- **Est effort:** L (~16 jam)
- **Dependencies:** E1.

## E3 — Mock Provider + Tier A Tests

- **Phase:** 1a
- **User Stories:**
  - US3.1: Sebagai developer, saya ingin mock LLM provider (scripted responses), agar agent loop dapat diuji tanpa API key/network.
- **Acceptance Criteria:**
  - [ ] Mock provider mengikuti trait yang sama dengan provider riil.
  - [ ] Tier A tests (fast, deterministic, no network) meng-cover: agent loop, state transitions, storage roundtrip.
  - [ ] `cargo test --features mock-provider` hijau < 30 detik di CI.
- **Est effort:** M (~10 jam)
- **Dependencies:** E2.

## E4 — `cora search` Tool (ReadOnly) + Real LLM Provider

- **Phase:** 1b
- **User Stories:**
  - US4.1: Sebagai agent, saya ingin tool `cora search` (ReadOnly, memanggil cora brain/code search), agar dapat query context codebase tanpa risiko mutasi.
  - US4.2: Sebagai user, saya ingin real LLM provider (trait impl, mis. HTTP client ke provider konfigurabel), agar agent berjalan dengan model nyata.
- **Acceptance Criteria:**
  - [ ] Tool registry meng-enforce `ReadOnly` permission — tool write-capable ditolak register tanpa approval gate (test).
  - [ ] Real provider terkonfigurasi via env/config; API key tidak pernah di-log (test grep log output).
  - [ ] Integration test (Tier B, network-optional, di-skip tanpa key): single turn end-to-end — prompt → provider → tool call `cora search` → jawaban.
- **Est effort:** L (~14 jam)
- **Dependencies:** E3.

## E5 — End-to-End Single Turn + Crash-Resume Test

- **Phase:** 1b
- **User Stories:**
  - US5.1: Sebagai developer, saya ingin test end-to-end single turn penuh (mock provider) + crash-resume, agar jaminan determinisme E2 terbukti di level integration.
- **Acceptance Criteria:**
  - [ ] Scenario test: kill process mid-turn → restart → run selesai dengan hasil identik run normal (golden-file compare).
  - [ ] Test masuk CI sebagai Tier A (pakai mock provider).
- **Est effort:** M (~8 jam)
- **Dependencies:** E2, E4.

## E6 — Approval Gates (Approver Trait + Allowlist + Interactive CLI)

- **Phase:** 2
- **User Stories:**
  - US6.1: Sebagai user, saya ingin setiap tool Write/execute melewati `Approver` trait, agar aksi berbahaya selalu butuh persetujuan.
  - US6.2: Sebagai user, saya ingin impl `AllowlistApprover` (pattern-based) dan `InteractiveApprover` (prompt y/N di CLI), agar bisa pilih mode otomatis-terbatas atau manual.
- **Acceptance Criteria:**
  - [ ] Semua tool non-ReadOnly wajib Approver; bypass = compile-time/test-time error path (test: unapproved write tidak dieksekusi, event `denied` tercatat).
  - [ ] Allowlist: glob/pattern match unit tests.
  - [ ] Interactive: prompt di CLI menampilkan command + diff ringkas sebelum y/N.
- **Est effort:** M (~12 jam)
- **Dependencies:** E4.

## E7 — Tools Tambahan: uteke search (RO), file read (RO), gh CLI (Write)

- **Phase:** 2
- **User Stories:**
  - US7.1: Sebagai agent, saya ingin tool `uteke search` (ReadOnly) untuk query memori, tool `file read` (ReadOnly), dan tool `gh` CLI (Write: issue comment, PR create), agar mampu memperbaiki GitHub issue end-to-end.
- **Acceptance Criteria:**
  - [ ] 3 tool terimplementasi dengan test masing-masing (gh via dry-run/fake binary).
  - [ ] `gh` Write selalu lewat Approver (E6).
  - [ ] Path traversal pada file read ditolak (test).
- **Est effort:** M (~12 jam)
- **Dependencies:** E6.

## E8 — MVP Gate: Perbaiki 1 GitHub Issue Riil End-to-End

- **Phase:** 2 (gate)
- **User Stories:**
  - US8.1: Sebagai maintainer, saya ingin agent memperbaiki 1 issue riil (reproduce → fix → test → PR), agar MVP terbukti bernilai.
- **Acceptance Criteria:**
  - [ ] Run terdokumentasi (log + session replay) di issue nyata di repo sendiri.
  - [ ] PR dibuat via `gh` tool, CI hijau, PR merged (atau di-review manual lalu merge).
  - [ ] Write-up retro: apa yang kurang → backlog Phase 3.
- **Est effort:** M (~10 jam, termasuk intervensi manual yang dicatat)
- **Dependencies:** E7, E5.

## E9 — Hardening, Cross-Build CI, Perf, OSS Prep

- **Phase:** 3
- **User Stories:**
  - US9.1: Sebagai maintainer, saya ingin cross-build CI target `aarch64-android` dan `aarch64-apple-ios` hijau, agar jalur mobile terbukti.
  - US9.2: Sebagai maintainer, saya ingin hardening (error handling, timeout tool, limit output) + benchmark perf dasar, agar siap pengguna luar.
  - US9.3: Sebagai pengguna luar, saya ingin OSS prep (LICENSE, README, CONTRIBUTING, publish crate/binary), agar bisa coba.
- **Acceptance Criteria:**
  - [ ] CI matrix build 2 target cross tersebut success (build saja, tidak run).
  - [ ] Tool timeout + output truncation teruji; panic-free pada input jelek (fuzz-lite/manual).
  - [ ] Benchmark: single turn mock < threshold terdokumen; dokumen hasil di `docs/perf.md`.
  - [ ] Repo public-ready: LICENSE, README quickstart, SECURITY.md.
- **Est effort:** L (~16 jam)
- **Dependencies:** E8.

---

## Sprint Plan (6 minggu, ~10 jam/minggu)

| Minggu | Fokus | Epic | Deliverable | Checkpoint Review (akhir minggu) | Kill / Pivot Criteria |
|---|---|---|---|---|---|
| W1 | Phase 1a — Storage | E1 | 3-store schema + migrate-on-open + test | Migrasi roundtrip hijau; schema doc ada | Jika SQLite abstraksi terbukti >2x overbudget (>20 jam), sederhanakan ke 1 file DB multi-table — jangan tunda. |
| W2 | Phase 1a — State Machine + Mock | E2, E3 | Seq CAS, effect sandwich, mock provider, Tier A hijau | Crash-resume unit-level deterministik; `cargo test` hijau CI | Jika determinisme replay gagal terus >1 minggu ekstra: kill — arsitektur core tidak sound. |
| W3 | Phase 1b — cora search + real provider | E4, E5 | ReadOnly tool + real LLM provider + E2E single turn + crash-resume test | E2E single turn (mock & real-if-key) hijau | Jika real provider flaky >50% run: ganti provider utama, jangan debug tanpa batas. |
| W4 | Phase 2 — Approval Gates | E6 | Approver trait + Allowlist + Interactive CLI + tests | Demo CLI: write request → prompt y/N → denied tercatat | Jika desain Approver mulai creep ke RBAC kompleks: cut, cukup 2 impl. |
| W5 | Phase 2 — Tools + MVP Gate | E7, E8 | uteke search, file read, gh Write + **MVP: 1 issue riil diperbaiki** | PR dari agent merged + retro write-up | **MVP gate:** jika setelah 2 sesi intervensi agent masih gagal total, evaluasi lanjut vs stop — jangan lanjut Phase 3 otomatis. |
| W6 | Phase 3 — Hardening | E9 | Cross-build CI (aarch64-android, aarch64-apple-ios), timeout/truncation, perf doc, OSS prep | CI matrix hijau, LICENSE+README ready | Jika cross-build gagal karena dependency C: catat blocker, defer target — tidak boleh geser >W7. |

### Catatan alokasi

- Buffer: setiap minggu sisakan ~2 jam untuk review checkpoint + backlog grooming.
- Total estimasi: E1–E9 ≈ 110 jam vs kapasitas 60 jam → **estimasi effort adalah effort ideal; jika W5 MVP gate belum tercapai, W6 (Phase 3) ditunda, bukan dipadatkan.** MVP gate adalah satu-satunya deadline keras.
