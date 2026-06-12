# Security Policy

Remora's core premise is security-driven: the agent, the checked-out code, and
all tool execution stay on a remote sandbox, never on your device. We take
reports that undermine that boundary especially seriously — e.g. anything that
lets session content escape the sandbox, leaks credentials to a client device,
or lets one session/relay user reach another's session.

## Supported versions

Pre-1.0, only the latest release receives security fixes.

## Reporting a vulnerability

**Do not open a public issue.** Instead, use
[GitHub private vulnerability reporting](https://github.com/nnayda/remora/security/advisories/new).

Please include reproduction steps, the affected component (desktop app, relay,
protocol), and impact. You can expect an acknowledgement within a few days.
We'll coordinate disclosure with you; we ask that you give us a reasonable
window to ship a fix before publishing details.

## Scope notes

- Sandbox hardening (resource limits, egress policy, credential hygiene) is
  the operator's responsibility — see the security invariants in
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Reports about a misconfigured sandbox
  itself are out of scope; reports about Remora *encouraging* an insecure
  default are in scope.
