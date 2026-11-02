use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::{
    env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseOrValue,
    PromiseResult,
};

/// Items a learner must newly master before a milestone payout is released.
pub const MILESTONE_SIZE: u32 = 50;
/// Largest jump in a learner's mastered count accepted in one report.
/// Guards against a compromised oracle claiming implausible progress.
pub const MAX_ITEMS_PER_REPORT: u32 = 150;
const DAY_NS: u64 = 86_400_000_000_000;
const FT_TRANSFER_GAS: Gas = Gas::from_tgas(15);
const CALLBACK_GAS: Gas = Gas::from_tgas(10);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Centre {
    pub name: String,
    pub payee: AccountId,
    pub learners: u32,
    pub milestones_paid: u32,
    pub total_paid: NearToken,
    pub day: u64,
    pub paid_today: NearToken,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct LearningFund {
    owner: AccountId,
    /// Account allowed to report learner progress (the Kratu sync service).
    oracle: AccountId,
    /// Native NEAR (in yocto) or, in token mode, raw token units.
    reward_per_milestone: NearToken,
    /// If set, the fund holds and pays this NEP-141 token (e.g. a stablecoin)
    /// instead of native NEAR. All amounts are then raw token units.
    token: Option<AccountId>,
    /// Token units held for the pool (token mode only).
    token_balance: u128,
    /// Most a single centre can receive per UTC day.
    daily_cap_per_centre: NearToken,
    paused: bool,
    centres: IterableMap<String, Centre>,
    /// "centre_id:learner_hash" -> highest mastered-item count already rewarded.
    rewarded: LookupMap<String, u32>,
    total_donated: NearToken,
    total_paid: NearToken,
}

#[near]
impl LearningFund {
    #[init]
    pub fn new(
        owner: AccountId,
        oracle: AccountId,
        reward_per_milestone: NearToken,
        daily_cap_per_centre: NearToken,
        token: Option<AccountId>,
    ) -> Self {
        Self {
            owner,
            oracle,
            reward_per_milestone,
            daily_cap_per_centre,
            token,
            token_balance: 0,
            paused: false,
            centres: IterableMap::new(b"c"),
            rewarded: LookupMap::new(b"r"),
            total_donated: NearToken::from_yoctonear(0),
            total_paid: NearToken::from_yoctonear(0),
        }
    }

    /// Anyone can fund the pool.
    #[payable]
    pub fn donate(&mut self) {
        require!(self.token.is_none(), "This fund takes token donations via ft_transfer_call");
        let amount = env::attached_deposit();
        require!(!amount.is_zero(), "Attach a deposit to donate");
        self.total_donated = self.total_donated.saturating_add(amount);
    }

    /// NEP-141 receiver: donors call `ft_transfer_call` on the token with this
    /// contract as receiver. Anything but the configured token is refused.
    #[allow(unused_variables)]
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        let token = self.token.as_ref().expect("This fund is not in token mode");
        require!(&env::predecessor_account_id() == token, "Unsupported token");
        self.token_balance += amount.0;
        self.total_donated = self.total_donated.saturating_add(NearToken::from_yoctonear(amount.0));
        PromiseOrValue::Value(U128(0))
    }

    pub fn register_centre(&mut self, centre_id: String, name: String, payee: AccountId) {
        self.assert_owner();
        require!(!self.centres.contains_key(&centre_id), "Centre already registered");
        self.centres.insert(
            centre_id,
            Centre {
                name,
                payee,
                learners: 0,
                milestones_paid: 0,
                total_paid: NearToken::from_yoctonear(0),
                day: 0,
                paid_today: NearToken::from_yoctonear(0),
            },
        );
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.assert_owner();
        self.paused = paused;
    }

    pub fn set_oracle(&mut self, oracle: AccountId) {
        self.assert_owner();
        self.oracle = oracle;
    }

    /// Two-step safety is left to the caller: double-check the account id.
    pub fn transfer_ownership(&mut self, new_owner: AccountId) {
        self.assert_owner();
        self.owner = new_owner;
    }

    /// Owner can pull unspent funds back, e.g. to wind the pilot down.
    /// Keeps enough balance to cover contract storage.
    pub fn withdraw(&mut self, amount: NearToken) -> Promise {
        self.assert_owner();
        require!(self.available_balance() >= amount.as_yoctonear(), "Insufficient funds");
        if self.token.is_some() {
            self.token_balance -= amount.as_yoctonear();
        }
        self.pay(self.owner.clone(), amount)
    }

    pub fn set_reward(&mut self, reward_per_milestone: NearToken) {
        self.assert_owner();
        self.reward_per_milestone = reward_per_milestone;
    }

    /// Called by the oracle with a learner's cumulative mastered-item count.
    /// `learner_hash` is an opaque identifier; no personal data goes on-chain.
    /// Pays the centre once per newly crossed milestone. Replays and
    /// out-of-order reports cannot pay twice because only the high-water mark
    /// advances. Returns the number of milestones paid by this call.
    pub fn report_progress(
        &mut self,
        centre_id: String,
        learner_hash: String,
        items_mastered: u32,
    ) -> u32 {
        require!(!self.paused, "Fund is paused");
        require!(env::predecessor_account_id() == self.oracle, "Only the oracle can report");
        let mut centre = self.centres.get(&centre_id).cloned().expect("Unknown centre");

        let key = format!("{centre_id}:{learner_hash}");
        let previous = self.rewarded.get(&key).copied();
        if previous.is_none() {
            centre.learners += 1;
        }
        let previous = previous.unwrap_or(0);

        require!(
            items_mastered <= previous.saturating_add(MAX_ITEMS_PER_REPORT),
            "Reported progress jump is implausibly large"
        );

        let new_milestones = (items_mastered / MILESTONE_SIZE)
            .saturating_sub(previous / MILESTONE_SIZE);

        if items_mastered > previous {
            self.rewarded.insert(key.clone(), items_mastered);
            if new_milestones > 0 {
                let owed = self.reward_per_milestone.as_yoctonear() * new_milestones as u128;
                require!(self.available_balance() >= owed, "Pool has insufficient funds");
                let amount = NearToken::from_yoctonear(owed);
                let today = env::block_timestamp() / DAY_NS;
                if centre.day != today {
                    centre.day = today;
                    centre.paid_today = NearToken::from_yoctonear(0);
                }
                require!(
                    centre.paid_today.saturating_add(amount) <= self.daily_cap_per_centre,
                    "Centre daily cap reached"
                );
                centre.paid_today = centre.paid_today.saturating_add(amount);
                centre.milestones_paid += new_milestones;
                centre.total_paid = centre.total_paid.saturating_add(amount);
                self.total_paid = self.total_paid.saturating_add(amount);
                let payee = centre.payee.clone();
                let centre_id_for_cb = centre_id.clone();
                self.centres.insert(centre_id, centre);
                if self.token.is_some() {
                    self.token_balance -= owed;
                }
                self.pay(payee, amount).then(
                    Self::ext(env::current_account_id())
                        .with_static_gas(CALLBACK_GAS)
                        .on_payout(centre_id_for_cb, key, previous, amount, new_milestones),
                );
                return new_milestones;
            }
        }
        self.centres.insert(centre_id, centre);
        0
    }

    /// If the transfer to the centre failed (e.g. the payee is not registered
    /// with the token), undo the accounting so the learner can be paid later.
    #[private]
    #[allow(deprecated)]
    pub fn on_payout(
        &mut self,
        centre_id: String,
        key: String,
        previous: u32,
        amount: NearToken,
        milestones: u32,
    ) {
        if matches!(env::promise_result(0), PromiseResult::Successful(_)) {
            return;
        }
        env::log_str("Payout failed; progress and totals rolled back");
        if previous == 0 {
            self.rewarded.remove(&key);
        } else {
            self.rewarded.insert(key, previous);
        }
        if let Some(mut centre) = self.centres.get(&centre_id).cloned() {
            if previous == 0 {
                centre.learners = centre.learners.saturating_sub(1);
            }
            centre.milestones_paid = centre.milestones_paid.saturating_sub(milestones);
            centre.total_paid = NearToken::from_yoctonear(
                centre.total_paid.as_yoctonear().saturating_sub(amount.as_yoctonear()),
            );
            centre.paid_today = NearToken::from_yoctonear(
                centre.paid_today.as_yoctonear().saturating_sub(amount.as_yoctonear()),
            );
            self.centres.insert(centre_id, centre);
        }
        self.total_paid = NearToken::from_yoctonear(
            self.total_paid.as_yoctonear().saturating_sub(amount.as_yoctonear()),
        );
        if self.token.is_some() {
            self.token_balance += amount.as_yoctonear();
        }
    }

    // ---- views ----

    /// The NEP-141 token this fund pays in, or None for native NEAR.
    pub fn get_token(&self) -> Option<AccountId> {
        self.token.clone()
    }

    pub fn get_config(&self) -> (AccountId, AccountId, NearToken, u32) {
        (self.owner.clone(), self.oracle.clone(), self.reward_per_milestone, MILESTONE_SIZE)
    }

    pub fn get_totals(&self) -> (NearToken, NearToken, NearToken) {
        let held = if self.token.is_some() {
            NearToken::from_yoctonear(self.token_balance)
        } else {
            env::account_balance()
        };
        (self.total_donated, self.total_paid, held)
    }

    pub fn get_centre(&self, centre_id: String) -> Option<Centre> {
        self.centres.get(&centre_id).cloned()
    }

    pub fn list_centres(&self) -> Vec<(String, Centre)> {
        self.centres.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn get_learner_progress(&self, centre_id: String, learner_hash: String) -> u32 {
        self.rewarded.get(&format!("{centre_id}:{learner_hash}")).copied().unwrap_or(0)
    }

    /// Sends `amount` to `to` in the fund's currency. In token mode the payee
    /// must already be registered with the token contract (storage deposit).
    fn pay(&self, to: AccountId, amount: NearToken) -> Promise {
        match &self.token {
            None => Promise::new(to).transfer(amount),
            Some(token) => Promise::new(token.clone()).function_call(
                "ft_transfer".to_string(),
                json!({ "receiver_id": to, "amount": amount.as_yoctonear().to_string() })
                    .to_string()
                    .into_bytes(),
                ONE_YOCTO,
                FT_TRANSFER_GAS,
            ),
        }
    }

    fn available_balance(&self) -> u128 {
        if self.token.is_some() {
            return self.token_balance;
        }
        env::account_balance()
            .as_yoctonear()
            .saturating_sub(env::storage_byte_cost().as_yoctonear() * env::storage_usage() as u128)
    }

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Only the owner can do this");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    fn acct(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(caller: &str, deposit: NearToken, balance: NearToken) -> VMContextBuilder {
        let mut b = VMContextBuilder::new();
        b.predecessor_account_id(acct(caller))
            .attached_deposit(deposit)
            .account_balance(balance);
        b
    }

    fn setup() -> LearningFund {
        testing_env!(ctx("owner.testnet", NearToken::from_near(0), NearToken::from_near(100)).build());
        let mut c = LearningFund::new(acct("owner.testnet"), acct("oracle.testnet"), NearToken::from_near(1), NearToken::from_near(10), None);
        c.register_centre("potiskum-1".into(), "Potiskum Centre".into(), acct("centre.testnet"));
        c
    }

    fn as_oracle(balance: u128) {
        testing_env!(ctx("oracle.testnet", NearToken::from_near(0), NearToken::from_near(balance)).build());
    }

    #[test]
    fn pays_once_per_milestone() {
        let mut c = setup();
        as_oracle(100);
        assert_eq!(c.report_progress("potiskum-1".into(), "abc".into(), 49), 0);
        assert_eq!(c.report_progress("potiskum-1".into(), "abc".into(), 50), 1);
        assert_eq!(c.report_progress("potiskum-1".into(), "abc".into(), 120), 1);
        assert_eq!(c.get_totals().1, NearToken::from_near(2));
        assert_eq!(c.get_centre("potiskum-1".into()).unwrap().learners, 1);
    }

    #[test]
    fn replay_and_regression_do_not_repay() {
        let mut c = setup();
        as_oracle(100);
        c.report_progress("potiskum-1".into(), "abc".into(), 100);
        assert_eq!(c.report_progress("potiskum-1".into(), "abc".into(), 100), 0);
        assert_eq!(c.report_progress("potiskum-1".into(), "abc".into(), 60), 0);
        assert_eq!(c.get_learner_progress("potiskum-1".into(), "abc".into()), 100);
        assert_eq!(c.get_totals().1, NearToken::from_near(2));
    }

    #[test]
    #[should_panic(expected = "Only the oracle can report")]
    fn only_oracle_reports() {
        let mut c = setup();
        testing_env!(ctx("mallory.testnet", NearToken::from_near(0), NearToken::from_near(100)).build());
        c.report_progress("potiskum-1".into(), "abc".into(), 500);
    }

    #[test]
    #[should_panic(expected = "Pool has insufficient funds")]
    fn cannot_overspend_pool() {
        let mut c = setup();
        testing_env!(ctx("oracle.testnet", NearToken::from_near(0), NearToken::from_millinear(500)).build());
        c.report_progress("potiskum-1".into(), "abc".into(), 50);
    }

    #[test]
    #[should_panic(expected = "implausibly large")]
    fn rejects_implausible_jump() {
        let mut c = setup();
        as_oracle(100);
        c.report_progress("potiskum-1".into(), "abc".into(), MAX_ITEMS_PER_REPORT + 1);
    }

    #[test]
    #[should_panic(expected = "Centre daily cap reached")]
    fn enforces_daily_cap() {
        let mut c = setup(); // cap 10 NEAR, reward 1 NEAR per milestone
        as_oracle(100);
        for i in 0..4 {
            c.report_progress("potiskum-1".into(), format!("l{i}"), 150); // 3 NEAR each
        }
    }

    #[test]
    #[should_panic(expected = "Fund is paused")]
    fn pause_blocks_reports() {
        let mut c = setup();
        c.set_paused(true);
        as_oracle(100);
        c.report_progress("potiskum-1".into(), "abc".into(), 50);
    }

    #[test]
    fn owner_can_transfer_and_withdraw() {
        let mut c = setup();
        c.transfer_ownership(acct("new.testnet"));
        testing_env!(ctx("new.testnet", NearToken::from_near(0), NearToken::from_near(100)).build());
        c.withdraw(NearToken::from_near(5));
        assert_eq!(c.get_config().0, acct("new.testnet"));
    }

    fn token_fund() -> LearningFund {
        testing_env!(ctx("owner.testnet", NearToken::from_near(0), NearToken::from_near(1)).build());
        let mut c = LearningFund::new(
            acct("owner.testnet"),
            acct("oracle.testnet"),
            NearToken::from_yoctonear(1_000_000),
            NearToken::from_yoctonear(10_000_000),
            Some(acct("usdc.testnet")),
        );
        c.register_centre("potiskum-1".into(), "Potiskum Centre".into(), acct("centre.testnet"));
        c
    }

    #[test]
    fn token_donations_and_payouts() {
        let mut c = token_fund();
        testing_env!(ctx("usdc.testnet", NearToken::from_near(0), NearToken::from_near(1)).build());
        c.ft_on_transfer(acct("donor.testnet"), U128(5_000_000), String::new());
        assert_eq!(c.get_totals().2, NearToken::from_yoctonear(5_000_000));
        testing_env!(ctx("oracle.testnet", NearToken::from_near(0), NearToken::from_near(1)).build());
        assert_eq!(c.report_progress("potiskum-1".into(), "abc".into(), 100), 2);
        let (donated, paid, held) = c.get_totals();
        assert_eq!(donated, NearToken::from_yoctonear(5_000_000));
        assert_eq!(paid, NearToken::from_yoctonear(2_000_000));
        assert_eq!(held, NearToken::from_yoctonear(3_000_000));
    }

    #[test]
    #[should_panic(expected = "Unsupported token")]
    fn rejects_other_tokens() {
        let mut c = token_fund();
        testing_env!(ctx("fake.testnet", NearToken::from_near(0), NearToken::from_near(1)).build());
        c.ft_on_transfer(acct("donor.testnet"), U128(1), String::new());
    }

    #[test]
    #[should_panic(expected = "Pool has insufficient funds")]
    fn token_pool_cannot_overspend() {
        let mut c = token_fund();
        testing_env!(ctx("oracle.testnet", NearToken::from_near(0), NearToken::from_near(1)).build());
        c.report_progress("potiskum-1".into(), "abc".into(), 50);
    }

    #[test]
    #[should_panic(expected = "token donations via ft_transfer_call")]
    fn native_donate_blocked_in_token_mode() {
        let mut c = token_fund();
        testing_env!(ctx("donor.testnet", NearToken::from_near(1), NearToken::from_near(2)).build());
        c.donate();
    }

    #[test]
    fn donations_tracked() {
        let mut c = setup();
        testing_env!(ctx("donor.testnet", NearToken::from_near(5), NearToken::from_near(105)).build());
        c.donate();
        assert_eq!(c.get_totals().0, NearToken::from_near(5));
    }
}
