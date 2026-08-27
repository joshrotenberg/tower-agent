# Documentation

- [`architecture.md`](architecture.md): service laws, layer ordering, provider
  lifecycle, and middleware opportunities.
- [`plan.md`](plan.md): planning-crate decision record, precedence and merge
  laws, and the retargeting away from argv compilation.
- [`mechanical-steps.md`](mechanical-steps.md): the boundary a subprocess
  step runner would have to hold, and why the decision is to defer one.
- [`workflow-host-report.md`](workflow-host-report.md): what the workflow
  library felt like from a real configuration-driven host, and what that
  says about publishing it.
- [`resilience.md`](resilience.md): which resilience policies are safe over an
  agent turn, what each requires first, and the recommended layer order.
- [`../README.md`](../README.md): workspace overview and usage.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md): local checks and change discipline.
- Rustdoc and tests under `crates/tower-agent`: executable API contract.

When prose and executable behavior disagree, treat it as a bug. Tests establish
implemented behavior; architecture notes identify deliberately open seams.
