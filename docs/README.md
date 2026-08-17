# Documentation

- [`architecture.md`](architecture.md): service laws, layer ordering, provider
  lifecycle, and middleware opportunities.
- [`../README.md`](../README.md): workspace overview and usage.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md): local checks and change discipline.
- Rustdoc and tests under `crates/tower-agent`: executable API contract.

When prose and executable behavior disagree, treat it as a bug. Tests establish
implemented behavior; architecture notes identify deliberately open seams.
