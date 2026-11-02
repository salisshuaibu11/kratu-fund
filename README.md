# Kratu Learning Fund

Outcome-based funding for learning centres on NEAR. Donors fill a pool; money is released to a centre only when children there master English items in [Kratu](https://www.kratu.ng/), the offline Hausa-first tutor for children aged 6-10.

**Pay for learning, not attendance.**

## How it works

1. Anyone calls `donate` and funds the pool.
2. The owner registers centres (`register_centre`) with a payee account.
3. An oracle account (the Kratu sync service) calls `report_progress(centre_id, learner_hash, items_mastered)` with a learner's cumulative count.
4. For every 50 newly mastered items the contract pays the centre `reward_per_milestone`.
5. Replays and lower counts never pay twice: only a per-learner high-water mark advances.
6. Guards against a bad or compromised oracle: a single report may raise a learner's count by at most 150 items, and each centre has a daily payout cap. The owner can pause the fund, rotate the oracle, transfer ownership and withdraw unspent funds.
7. A static dashboard reads totals and per-centre payouts straight from the contract.

No personal data goes on-chain. Learners are opaque hashes; Kratu's face patterns never leave the device.

## Honest status

- **Kratu has no public API.** `oracle/feed.json` is **mock data** shaped like the pilot records Kratu says its controller syncs when online. Nothing here is affiliated with or endorsed by Kratu.
- The oracle is a trusted account. A production version would need attestation of the tablets (e.g. TEE) so the oracle cannot invent progress.
- Payouts are in native NEAR. Stablecoin payouts via NEP-141 / NEAR Intents are the obvious next step.
- Contract logic has 9 passing unit tests. It is deployed on NEAR testnet at `kratu-fund-salisu.testnet` (reward 0.1 NEAR per 50 items, daily cap 1 NEAR per centre) and was exercised end to end: two centres registered, 2 NEAR donated, the mock feed synced, and 8 milestones (0.8 NEAR) recorded as paid. In that run the owner, oracle and payee were all the same account.

## Roadmap

1. Stablecoin payouts (NEP-141, cross-chain donations via NEAR Intents).
2. Donor governance over reward rate and large payouts (SputnikDAO / Trezu).
3. Tablet attestation (TEE) so the oracle cannot fabricate progress.
4. Privacy-preserving learner credential badges (NEP-171).
5. Dashboard: wallet connect, donate button, payout history.

## Run it

```bash
# contract tests
cd contract && cargo test

# build wasm (needs: rustup target add wasm32-unknown-unknown, and cargo-near)
cargo near build

# deploy (init args: owner, oracle, reward per milestone, daily cap per centre, in yoctoNEAR)
# near contract deploy <acct>.testnet use-file target/wasm32-unknown-unknown/release/kratu_fund.wasm \
#   with-init-call new json-args '{"owner":"<acct>.testnet","oracle":"<acct>.testnet","reward_per_milestone":"1000000000000000000000000","daily_cap_per_centre":"10000000000000000000000000"}' \
#   prepaid-gas '30 Tgas' attached-deposit '0 NEAR' network-config testnet sign-with-keychain send

# dry-run the oracle against the mock feed
node oracle/sync.js kratu-fund.testnet oracle.testnet

# dashboard
open dashboard/index.html
```

Built for the [NEAR Hackathon](https://nearhackathon.com/), tracks: Social Money, Real-World Money.
