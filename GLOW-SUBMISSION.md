# Glow submission — XRPL Rust Validator + native transaction engine

**Repo:** https://github.com/ilubxrp589/xrpl-validator
**License:** MIT (open-source, freely available)
**Contributor:** ilubxrp589

Paste-ready notes for the Glow application form (glow.xrpl-commons.org).
Every claim here is verifiable against the repo's `git log`, README, and
`scripts/corpus.sh`.

---

## One-line summary

An independent XRP Ledger validator written in Rust that verifies mainnet
state hashes every ledger, plus a from-scratch native Rust transaction
engine validated byte-for-byte against rippled on real mainnet ledgers — a
step toward XRPL client/implementation diversity.

## What this contributes to the XRPL (mapped to Glow categories)

- **Supporting infrastructure / core protocol** — A second, independent
  validator implementation. XRPL consensus today runs almost entirely on one
  codebase (rippled); an independent Rust node that recomputes and verifies
  `account_hash` every ledger is a genuine resilience and
  implementation-diversity contribution. Documented run: **28,500+
  consecutive mainnet hash matches, zero mismatches.**

- **Improving core protocol / solving technical debt** — A from-scratch
  native Rust transaction engine (`crates/xrpl-ledger`) covering Payment,
  DEX/OfferCreate, AMM (+swap), Check, Escrow, PayChannel, Ticket, TrustSet,
  NFToken, Credential, and Oracle transactors. It is proven correct against
  rippled by a **differential harness** that replays real mainnet
  transactions and compares every result to the canonical outcome. Reached
  **100% attempted-transaction parity on a 19-ledger mainnet corpus**, with a
  git-anchored history of specific parity fixes (e.g. issuer `TransferRate`
  on delivery, `keepRoot` directory teardown, `CheckCancel` dual-directory
  unlink, underfunded-maker crossing).

- **Community tool** — The differential harness itself (`differential_probe`
  + `scripts/corpus.sh`): a reproducible way to test any XRPL transaction
  engine for byte-exact agreement with rippled, ledger by ledger.

## Why it matters

XRPL has effectively one consensus implementation. A second, independently
written engine — verified for exactness against the reference — reduces
monoculture risk and is a concrete building block toward client diversity, a
long-standing goal for mature blockchains.

## Honest scope (stated up front)

- The **production** transaction-apply path is **libxrpl (rippled's engine)
  via FFI** — a deliberate, RippleX-recommended design. The native Rust
  engine is validated in the differential harness and is **not yet** the
  production path.
- A security review (conducted with the Fable 5 model) is being remediated on
  a dedicated track; unfixed-finding specifics are withheld.

## Eligibility checklist (Glow)

| Requirement | Status |
|---|---|
| Contributes directly to XRPL | ✅ validator + tx engine + test tooling |
| Freely available / open-source | ✅ MIT, public on GitHub |
| High-quality code + docs | ✅ README, PROGRESS log, tests, reproducible corpus |
| Meaningful impact / utility | ✅ implementation diversity + reusable correctness harness |
| Completed within last 6 months | ✅ active this month (git history) |
| Not previously funded | ✅ no prior grant/funding |
| Independent of employment | ✅ independent hobby/OSS work |

## Evidence links

- Repo + README: https://github.com/ilubxrp589/xrpl-validator
- Reproducible corpus gate: `scripts/corpus.sh`
- Parity-fix history: `git log` on `crates/xrpl-ledger`
- FFI architecture rationale: `ffi/ARCHITECTURE.md`
