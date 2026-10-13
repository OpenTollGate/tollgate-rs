# AGENTS.md — OpenTollGate/tollgate-rs

Instructions for AI coding agents. The cross-repository policy (ownership
map, verification hierarchy, agent principles) is
https://github.com/OpenTollGate/physical-router-test-automation/blob/main/docs/engineering-policy.md

## Cross-Repository Engineering Ownership and Verification

**This repository owns the Rust TollGate runtime implementation, its
internal architecture, unit tests and implementation-specific
documentation** — the same scope the Go implementation owns for itself.

Neighbors — do not duplicate them:

- The wire protocol is `OpenTollGate/tollgate` — implementations conform;
  they do not redefine it.
- Cross-implementation integration and deployed-artifact acceptance
  testing is owned by `OpenTollGate/physical-router-test-automation`
  (its dual-backend support exercises this implementation via
  `--backend rust`); unit tests stay here.
- Shipping packages are owned by `FreedomTechFeed/packages`.

Agent rules: behavior parity questions are settled by PRTA's
cross-implementation matrix, not by one implementation's local choice;
port-mapping, token-format and captive-portal contracts have one
declaration each — reference it, don't restate it.
