"""overseer eval rig v2 — playbook Ch.12 §4.1/4.2/4.5.

Modules:
  taskspec   task-spec v2 loading + validation (oracle required)
  graders    deterministic graders + oracle verification
  agents     solver drivers (overseer, mini control, oracle, fail)
  scheduler  seeded k-run matrix; infra/agent failure separation
  store      append-only results store
  stats      pass@1±CI, pass@k, pass^k, paired bootstrap, McNemar
  manifest   overseer manifest.json → RunRecord merge
  report     report card (markdown + machine JSON)
  benchmarks external benchmark adapters (tau2, lcb, docker-gated)
"""
