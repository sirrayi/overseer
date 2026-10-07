## 14. Computer Use and the Browser

### 14.1 The bar

OSWorld-Verified frontier results sit around 83–86%, mostly vendor-reported **[src]**. An accessibility snapshot costs a few hundred tokens against thousands for a screenshot **[src]**. Synara, built on the same cua-driver, adds foreground versus background consent, screen-lock revocation, a progress guard, settle-based waits, approval queue limits and a hard denylist **[code: synara probe, 2026-10-07]**.

### 14.2 Where we stand

After the lean pass on `fix2-computer` **[fix2]**: cua-driver is the only backend; every argument Overseer sends exists in the installed driver's schemas (v0.26.1) and in Synara's patched build; observations are lean (roles without the AX prefix, noise actions dropped, visible-depth indentation); model-visible results carry no audit fields; screenshots are sized to what the active model sees without provider downscaling, with one refit retry and refusal beyond; post-action reads only happen for observed windows; batches validate fully before dispatch; marks bind by ancestry path; modal dialogs gate actions. Nothing has ever run against a live desktop.

### 14.3 Workstreams

#### U1 — Live validation on the owner's Mac

**What.** Run the ignored `computer_live` test and a scripted live checklist after granting CuaDriver Accessibility and Screen Recording.

**Checklist.** Marks persist or churn as expected on real AX trees; `wait_for` latency and verify formats; modal roles in real apps; schema names; secret typing through the broker; screenshot sizes and image budgets at each model tier; zoom coordinates mapping back correctly; refusal codes (`target_not_on_active_space`, `auth_sheet_focused`, `stale_element_token` and others) surfaced usefully.

::: gate
Every checklist item passes or produces a filed finding; findings fixed before U2 merges.
:::

#### U2 — Consent modes for computer use

**What.** P5 applied: per-action mode asks once per task for screen reads and once for mutating actions; full automation never asks, with P6's sensitive-surface permissions and S4's shadow-verdict journal.

#### U3 — Progress guard

**What.** Synara's idea: count repeated refusals and unverified acts on the same target; after three, block that target for the task and tell the model why. Feeds the stuck detector as a computer-specific signal.

#### U4 — Settle waits

**What.** `wait_for` polls a predicate. Add `settle`: wait until the window's AX tree stops changing for a quiet interval (bounded), using the driver's settle support where the patched build provides it and AX-hash polling otherwise.

#### U5 — Clipboard

**What.** Read, write and paste as explicit actions, each its own permission category (clipboard contents are often secrets), with reads latching the untrusted taint.

#### U6 — Overseer's own computer helper

**What.** Today the owner installs CuaDriver.app separately. The target is an Overseer-managed helper app built from the MIT cua-driver crates at a pinned revision (patched like Synara's where useful), installed on first use. macOS attaches Accessibility and Screen Recording grants to a signed app bundle, so this needs stable signing: it waits on the Apple Developer account (owner action). Until then the external driver remains supported.

::: decision
The helper must stay a separate signed app; the harness binary itself must never hold Accessibility or Screen Recording grants, because macOS would attribute them to the terminal, extending screen and input access to every process run in it.
:::

#### U7 — Revocation on lock, sleep and session switch

**What.** Any standing computer-use grant dies when the screen locks, the machine sleeps, or the session switches, exactly as Synara does. Detected through the supervisor (R12) and session notifications.

#### U8 — The browser

**What.** Chromium-family browsers through CDP refs (have); others through the AX tree (have). Next: page snapshots as structured, token-lean text; navigation through the egress proxy (S1) for domain policy; per-origin grants (P4). The in-app WKWebView browser is Overseer Life's (Phase 1 of the Life plan); the harness provides the browser-agent protocol (element refs, actions, proofs) it implements.

#### U9 — Computer-use evaluation

**What.** A local OSWorld-style subset of tasks on the owner's Mac apps, run with owner presence (live desktop), measuring success, steps, tokens and image tokens per task, before and after each computer-use change.

#### U10 — GUI scripts through `run_code`

**What.** T2's computer reach: a script that observes and acts in a loop without each observation entering context. Waits on U1.

### 14.4 What could break

::: risk
**Real AX trees break assumptions made against the fake driver.** Mitigation: U1 before anything else ships; fixtures captured from real apps replace hand-written ones.
:::

::: risk
**Computer use in full automation reaches a password manager or payment page.** Mitigation within the owner's decision: P6 categories configured at onboarding, matched by bundle id and normalized name, and S4's audit.
:::
