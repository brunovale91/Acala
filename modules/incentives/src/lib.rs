// This file is part of Acala.

// Copyright (C) 2020-2021 Acala Foundation.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! # Incentives Module
//!
//! ## Overview
//!
//! Acala platform need support different types of rewards for some other protocol.
//! Each Pool has its own multi currencies rewards and reward accumulation
//! mechanism. ORML rewards module records the total shares, total multi currencies rewards anduser
//! shares of specific pool. Incentives module provides hooks to other protocals to manage shares,
//! accumulates rewards and distributes rewards to users based on their shares.
//!
//! Pool types:
//! 1. Loans: record the shares and rewards for users of Loans(Honzon protocol).
//! 2. Dex: record the shares and rewards for DEX makers who staking LP token.
//!
//! Rewards accumulation:
//! 1. Incentives: periodicly(AccumulatePeriod), accumulate fixed amount according to Incentive.
//! Rewards come from RewardsSource, please transfer enough tokens to RewardsSource before
//! start incentive plan.
//! 2. DexSaving: periodicly(AccumulatePeriod), the reward currency is Stable(KUSD/AUSD),
//! the accumulation amount is the multiplier of DexSavingRewardRates and the stable amount of
//! corresponding liquidity pool. CDPTreasury will issue the stable currency to RewardsSource.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::unused_unit)]
#![allow(clippy::upper_case_acronyms)]

use frame_support::{log, pallet_prelude::*, transactional, PalletId};
use frame_system::pallet_prelude::*;
use orml_traits::{Happened, MultiCurrency, RewardHandler};
use primitives::{Amount, Balance, CurrencyId};
use sp_runtime::{
	traits::{AccountIdConversion, One, UniqueSaturatedInto, Zero},
	DispatchResult, FixedPointNumber, RuntimeDebug,
};
use sp_std::{collections::btree_map::BTreeMap, prelude::*};
use support::{CDPTreasury, DEXIncentives, DEXManager, EmergencyShutdown, Rate};

mod mock;
mod tests;
pub mod weights;

pub use module::*;
pub use weights::WeightInfo;

/// PoolId for various rewards pools
#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, RuntimeDebug)]
pub enum PoolId {
	/// Rewards and shares pool for users who open CDP(CollateralCurrencyId)
	Loans(CurrencyId),

	/// Rewards and shares pool for DEX makers who stake LP token(LPCurrencyId)
	Dex(CurrencyId),
}

#[frame_support::pallet]
pub mod module {
	use super::*;

	#[pallet::config]
	pub trait Config:
		frame_system::Config
		+ orml_rewards::Config<Share = Balance, Balance = Balance, PoolId = PoolId, CurrencyId = CurrencyId>
	{
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The period to accumulate rewards
		#[pallet::constant]
		type AccumulatePeriod: Get<Self::BlockNumber>;

		/// The reward type for dex saving.
		#[pallet::constant]
		type StableCurrencyId: Get<CurrencyId>;

		/// The source account for native token rewards.
		#[pallet::constant]
		type RewardsSource: Get<Self::AccountId>;

		/// The origin which may update incentive related params
		type UpdateOrigin: EnsureOrigin<Self::Origin>;

		/// CDP treasury to issue rewards in stable token
		type CDPTreasury: CDPTreasury<Self::AccountId, Balance = Balance, CurrencyId = CurrencyId>;

		/// Currency for transfer/issue assets
		type Currency: MultiCurrency<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

		/// DEX to supply liquidity info
		type DEX: DEXManager<Self::AccountId, CurrencyId, Balance>;

		/// Emergency shutdown.
		type EmergencyShutdown: EmergencyShutdown;

		/// The module id, keep DexShare LP.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Share amount is not enough
		NotEnough,
		/// Invalid currency id
		InvalidCurrencyId,
		/// Invalid pool id
		InvalidPoolId,
		/// Invalid rate
		InvalidRate,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	#[pallet::metadata(T::AccountId = "AccountId", PoolId = "PoolId")]
	pub enum Event<T: Config> {
		/// Deposit DEX share. \[who, dex_share_type, deposit_amount\]
		DepositDexShare(T::AccountId, CurrencyId, Balance),
		/// Withdraw DEX share. \[who, dex_share_type, withdraw_amount\]
		WithdrawDexShare(T::AccountId, CurrencyId, Balance),
		/// Claim rewards. \[who, pool_id, reward_currency_id, actual_amount, deduction_amount\]
		ClaimRewards(T::AccountId, PoolId, CurrencyId, Balance, Balance),
		/// Incentive reward amount updated. \[pool_id, reward_currency_id,
		/// reward_amount_per_period\]
		IncentiveRewardAmountUpdated(PoolId, CurrencyId, Balance),
		/// Saving reward rate updated. \[pool_id, reward_rate_per_period\]
		SavingRewardRateUpdated(PoolId, Rate),
		/// Payout deduction rate updated. \[pool_id, deduction_rate\]
		ClaimRewardDeductionRateUpdated(PoolId, Rate),
	}

	/// Mapping from pool to its fixed incentive amounts of multi currencies per period.
	///
	/// IncentiveRewardAmounts: double_map Pool, RewardCurrencyId => RewardAmountPerPeriod
	#[pallet::storage]
	#[pallet::getter(fn incentive_reward_amounts)]
	pub type IncentiveRewardAmounts<T: Config> =
		StorageDoubleMap<_, Twox64Concat, PoolId, Twox64Concat, CurrencyId, Balance, ValueQuery>;

	/// Mapping from pool to its fixed reward rate per period.
	///
	/// DexSavingRewardRates: map Pool => SavingRatePerPeriod
	#[pallet::storage]
	#[pallet::getter(fn dex_saving_reward_rates)]
	pub type DexSavingRewardRates<T: Config> = StorageMap<_, Twox64Concat, PoolId, Rate, ValueQuery>;

	/// Mapping from pool to its claim reward deduction rate.
	///
	/// ClaimRewardDeductionRates: map Pool => DeductionRate
	#[pallet::storage]
	#[pallet::getter(fn claim_reward_deduction_rates)]
	pub type ClaimRewardDeductionRates<T: Config> = StorageMap<_, Twox64Concat, PoolId, Rate, ValueQuery>;

	/// The pending rewards amount, actual available rewards amount may be deducted
	///
	/// PendingMultiRewards: double_map PoolId, AccountId => BTreeMap<CurrencyId, Balance>
	#[pallet::storage]
	#[pallet::getter(fn pending_multi_rewards)]
	pub type PendingMultiRewards<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		PoolId,
		Twox64Concat,
		T::AccountId,
		BTreeMap<CurrencyId, Balance>,
		ValueQuery,
	>;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {
		fn on_initialize(now: T::BlockNumber) -> Weight {
			// accumulate reward periodically
			if now % T::AccumulatePeriod::get() == Zero::zero() {
				let mut count: u32 = 0;
				let shutdown = T::EmergencyShutdown::is_shutdown();

				for (pool_id, pool_info) in orml_rewards::PoolInfos::<T>::iter() {
					if !pool_info.total_shares.is_zero() {
						match pool_id {
							// do not accumulate incentives for PoolId::Loans after shutdown
							PoolId::Loans(_) if !shutdown => {
								count += 1;
								Self::accumulate_incentives(pool_id);
							}
							PoolId::Dex(lp_currency_id) => {
								// do not accumulate dex saving any more after shutdown
								if !shutdown {
									Self::accumulate_dex_saving(lp_currency_id, pool_id);
								}
								count += 1;
								Self::accumulate_incentives(pool_id);
							}
							_ => {}
						}
					}
				}

				T::WeightInfo::on_initialize(count)
			} else {
				0
			}
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Stake LP token to add shares of Pool::Dex
		///
		/// The dispatch origin of this call must be `Signed` by the transactor.
		///
		/// - `lp_currency_id`: LP token type
		/// - `amount`: amount to stake
		#[pallet::weight(<T as Config>::WeightInfo::deposit_dex_share())]
		#[transactional]
		pub fn deposit_dex_share(
			origin: OriginFor<T>,
			lp_currency_id: CurrencyId,
			#[pallet::compact] amount: Balance,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_deposit_dex_share(&who, lp_currency_id, amount)?;
			Ok(())
		}

		/// Unstake LP token to remove shares of Pool::Dex
		///
		/// The dispatch origin of this call must be `Signed` by the transactor.
		///
		/// - `lp_currency_id`: LP token type
		/// - `amount`: amount to unstake
		#[pallet::weight(<T as Config>::WeightInfo::withdraw_dex_share())]
		#[transactional]
		pub fn withdraw_dex_share(
			origin: OriginFor<T>,
			lp_currency_id: CurrencyId,
			#[pallet::compact] amount: Balance,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_withdraw_dex_share(&who, lp_currency_id, amount)?;
			Ok(())
		}

		/// Claim all avalible multi currencies rewards for specific PoolId.
		///
		/// The dispatch origin of this call must be `Signed` by the transactor.
		///
		/// - `pool_id`: pool type
		#[pallet::weight(<T as Config>::WeightInfo::claim_rewards())]
		#[transactional]
		pub fn claim_rewards(origin: OriginFor<T>, pool_id: PoolId) -> DispatchResult {
			let who = ensure_signed(origin)?;

			// orml_rewards will claim rewards for all currencies rewards
			<orml_rewards::Pallet<T>>::claim_rewards(&who, &pool_id);

			let pending_multi_rewards: BTreeMap<CurrencyId, Balance> = PendingMultiRewards::<T>::take(&pool_id, &who);
			let deduction_rate = Self::claim_reward_deduction_rates(&pool_id);

			for (currency_id, pending_reward) in pending_multi_rewards {
				if pending_reward.is_zero() {
					continue;
				}
				// calculate actual rewards and deduction amount
				let (actual_amount, deduction_amount) = {
					let deduction_amount = deduction_rate.saturating_mul_int(pending_reward).min(pending_reward);
					if !deduction_amount.is_zero() {
						// re-accumulate deduction to rewards pool if deduction amount is not zero
						<orml_rewards::Pallet<T>>::accumulate_reward(&pool_id, currency_id, deduction_amount)?;
					}
					(pending_reward.saturating_sub(deduction_amount), deduction_amount)
				};

				// transfer the actual reward(pending reward exclude deduction) to user from the pool. it should not
				// affect the process, ignore the result to continue. if it fails, just the user will not
				// be rewarded, there will not increase user balance.
				T::Currency::transfer(currency_id, &Self::account_id(), &who, actual_amount)?;

				Self::deposit_event(Event::ClaimRewards(
					who.clone(),
					pool_id,
					currency_id,
					actual_amount,
					deduction_amount,
				));
			}

			Ok(())
		}

		/// Update incentive reward amount for specific PoolId
		///
		/// The dispatch origin of this call must be `UpdateOrigin`.
		///
		/// - `updates`: Vec<(PoolId, Vec<(RewardCurrencyId, FixedAmountPerPeriod)>)>
		#[pallet::weight(<T as Config>::WeightInfo::update_incentive_rewards(
			updates.iter().fold(0, |count, x| count + x.1.len()) as u32
		))]
		#[transactional]
		pub fn update_incentive_rewards(
			origin: OriginFor<T>,
			updates: Vec<(PoolId, Vec<(CurrencyId, Balance)>)>,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin)?;
			for (pool_id, update_list) in updates {
				if let PoolId::Dex(currency_id) = pool_id {
					ensure!(currency_id.is_dex_share_currency_id(), Error::<T>::InvalidPoolId);
				}

				for (currency_id, amount) in update_list {
					IncentiveRewardAmounts::<T>::mutate_exists(pool_id, currency_id, |maybe_amount| {
						let mut v = maybe_amount.unwrap_or_default();
						if amount != v {
							v = amount;
							Self::deposit_event(Event::IncentiveRewardAmountUpdated(pool_id, currency_id, amount));
						}

						if v.is_zero() {
							*maybe_amount = None;
						} else {
							*maybe_amount = Some(v);
						}
					});
				}
			}
			Ok(())
		}

		/// Update DEX saving reward rate for specific PoolId
		///
		/// The dispatch origin of this call must be `UpdateOrigin`.
		///
		/// - `updates`: Vec<(PoolId, Rate)>
		#[pallet::weight(<T as Config>::WeightInfo::update_dex_saving_rewards(updates.len() as u32))]
		#[transactional]
		pub fn update_dex_saving_rewards(origin: OriginFor<T>, updates: Vec<(PoolId, Rate)>) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin)?;
			for (pool_id, rate) in updates {
				match pool_id {
					PoolId::Dex(currency_id) if currency_id.is_dex_share_currency_id() => {}
					_ => return Err(Error::<T>::InvalidPoolId.into()),
				}
				ensure!(rate <= Rate::one(), Error::<T>::InvalidRate);

				DexSavingRewardRates::<T>::mutate_exists(&pool_id, |maybe_rate| {
					let mut v = maybe_rate.unwrap_or_default();
					if rate != v {
						v = rate;
						Self::deposit_event(Event::SavingRewardRateUpdated(pool_id, rate));
					}

					if v.is_zero() {
						*maybe_rate = None;
					} else {
						*maybe_rate = Some(v);
					}
				});
			}
			Ok(())
		}

		/// Update claim rewards deduction rates for all rewards currencies of specific PoolId
		///
		/// The dispatch origin of this call must be `UpdateOrigin`.
		///
		/// - `updates`: Vec<(PoolId, DecutionRate>)>
		#[pallet::weight(<T as Config>::WeightInfo::update_claim_reward_deduction_rates(updates.len() as u32))]
		#[transactional]
		pub fn update_claim_reward_deduction_rates(
			origin: OriginFor<T>,
			updates: Vec<(PoolId, Rate)>,
		) -> DispatchResult {
			T::UpdateOrigin::ensure_origin(origin)?;
			for (pool_id, deduction_rate) in updates {
				if let PoolId::Dex(currency_id) = pool_id {
					ensure!(currency_id.is_dex_share_currency_id(), Error::<T>::InvalidPoolId);
				}
				ensure!(deduction_rate <= Rate::one(), Error::<T>::InvalidRate);
				ClaimRewardDeductionRates::<T>::mutate_exists(&pool_id, |maybe_rate| {
					let mut v = maybe_rate.unwrap_or_default();
					if deduction_rate != v {
						v = deduction_rate;
						Self::deposit_event(Event::ClaimRewardDeductionRateUpdated(pool_id, deduction_rate));
					}

					if v.is_zero() {
						*maybe_rate = None;
					} else {
						*maybe_rate = Some(v);
					}
				});
			}
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	pub fn account_id() -> T::AccountId {
		T::PalletId::get().into_account()
	}

	// accumulate incentive rewards of multi currencies
	fn accumulate_incentives(pool_id: PoolId) {
		for (reward_currency_id, reward_amount) in IncentiveRewardAmounts::<T>::iter_prefix(pool_id) {
			if reward_amount.is_zero() {
				continue;
			}

			let res = T::Currency::transfer(
				reward_currency_id,
				&T::RewardsSource::get(),
				&Self::account_id(),
				reward_amount,
			);

			match res {
				Ok(_) => {
					let _ = <orml_rewards::Pallet<T>>::accumulate_reward(
						&pool_id,
						reward_currency_id,
						reward_amount,
					)
					.map_err(|e| {
						log::error!(
							target: "incentives",
							"accumulate_reward: failed to accumulate reward to non-existen pool {:?}, reward_currency_id {:?}, reward_amount {:?}: {:?}",
							pool_id, reward_currency_id, reward_amount, e
						);
					});
				}
				Err(e) => {
					log::warn!(
						target: "incentives",
						"transfer: failed to transfer {:?} {:?} from {:?} to {:?}: {:?}. \
						This is unexpected but should be safe",
						reward_amount, reward_currency_id, T::RewardsSource::get(), Self::account_id(), e
					);
				}
			}
		}
	}

	// accumulate DEX saving reward(stable currency) for Dex Pool
	fn accumulate_dex_saving(lp_currency_id: CurrencyId, pool_id: PoolId) {
		let stable_currency_id = T::StableCurrencyId::get();
		let dex_saving_reward_rate = Self::dex_saving_reward_rates(&pool_id);

		if !dex_saving_reward_rate.is_zero() {
			if let Some((currency_id_a, currency_id_b)) = lp_currency_id.split_dex_share_currency_id() {
				// accumulate saving reward only for liquidity pool of stable currency id
				let dex_saving_reward_base = if currency_id_a == stable_currency_id {
					T::DEX::get_liquidity_pool(stable_currency_id, currency_id_b).0
				} else if currency_id_b == stable_currency_id {
					T::DEX::get_liquidity_pool(stable_currency_id, currency_id_a).0
				} else {
					Zero::zero()
				};
				let dex_saving_reward_amount = dex_saving_reward_rate.saturating_mul_int(dex_saving_reward_base);

				// issue stable currency without backing.
				if !dex_saving_reward_amount.is_zero() {
					let res = T::CDPTreasury::issue_debit(&Self::account_id(), dex_saving_reward_amount, false);
					match res {
						Ok(_) => {
							let _ = <orml_rewards::Pallet<T>>::accumulate_reward(
								&pool_id,
								stable_currency_id,
								dex_saving_reward_amount,
							)
							.map_err(|e| {
								log::error!(
									target: "incentives",
									"accumulate_reward: failed to accumulate reward to non-existen pool {:?}, reward_currency {:?}, amount {:?}: {:?}",
									pool_id, stable_currency_id, dex_saving_reward_amount, e
								);
							});
						}
						Err(e) => {
							log::warn!(
								target: "incentives",
								"issue_debit: failed to issue {:?} unbacked stable to {:?}: {:?}. \
								This is unexpected but should be safe",
								dex_saving_reward_amount, Self::account_id(), e
							);
						}
					}
				}
			}
		}
	}
}

impl<T: Config> DEXIncentives<T::AccountId, CurrencyId, Balance> for Pallet<T> {
	fn do_deposit_dex_share(who: &T::AccountId, lp_currency_id: CurrencyId, amount: Balance) -> DispatchResult {
		ensure!(lp_currency_id.is_dex_share_currency_id(), Error::<T>::InvalidCurrencyId);

		T::Currency::transfer(lp_currency_id, who, &Self::account_id(), amount)?;
		<orml_rewards::Pallet<T>>::add_share(who, &PoolId::Dex(lp_currency_id), amount.unique_saturated_into());

		Self::deposit_event(Event::DepositDexShare(who.clone(), lp_currency_id, amount));
		Ok(())
	}

	fn do_withdraw_dex_share(who: &T::AccountId, lp_currency_id: CurrencyId, amount: Balance) -> DispatchResult {
		ensure!(lp_currency_id.is_dex_share_currency_id(), Error::<T>::InvalidCurrencyId);
		ensure!(
			<orml_rewards::Pallet<T>>::shares_and_withdrawn_rewards(&PoolId::Dex(lp_currency_id), &who).0 >= amount,
			Error::<T>::NotEnough,
		);

		T::Currency::transfer(lp_currency_id, &Self::account_id(), who, amount)?;
		<orml_rewards::Pallet<T>>::remove_share(who, &PoolId::Dex(lp_currency_id), amount.unique_saturated_into());

		Self::deposit_event(Event::WithdrawDexShare(who.clone(), lp_currency_id, amount));
		Ok(())
	}
}

pub struct OnUpdateLoan<T>(sp_std::marker::PhantomData<T>);
impl<T: Config> Happened<(T::AccountId, CurrencyId, Amount, Balance)> for OnUpdateLoan<T> {
	fn happened(info: &(T::AccountId, CurrencyId, Amount, Balance)) {
		let (who, currency_id, adjustment, previous_amount) = info;
		let adjustment_abs =
			sp_std::convert::TryInto::<Balance>::try_into(adjustment.saturating_abs()).unwrap_or_default();

		let new_share_amount = if adjustment.is_positive() {
			previous_amount.saturating_add(adjustment_abs)
		} else {
			previous_amount.saturating_sub(adjustment_abs)
		};

		<orml_rewards::Pallet<T>>::set_share(who, &PoolId::Loans(*currency_id), new_share_amount);
	}
}

impl<T: Config> RewardHandler<T::AccountId, CurrencyId> for Pallet<T> {
	type Balance = Balance;
	type PoolId = PoolId;

	fn payout(who: &T::AccountId, pool_id: &Self::PoolId, currency_id: CurrencyId, payout_amount: Self::Balance) {
		if payout_amount.is_zero() {
			return;
		}
		PendingMultiRewards::<T>::mutate(pool_id, who, |rewards| {
			rewards
				.entry(currency_id)
				.and_modify(|current| *current = current.saturating_add(payout_amount))
				.or_insert(payout_amount);
		});
	}
}
