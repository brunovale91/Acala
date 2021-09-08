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

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::unused_unit)]

pub mod benchmarking;
mod mock;
mod tests;
pub mod weights;

use frame_support::{pallet_prelude::*, transactional};
use frame_system::{ensure_signed, pallet_prelude::*};
use module_support::{CallBuilder, ExchangeRate, Ratio};
use orml_traits::{BalanceStatus, MultiCurrency, MultiReservableCurrency, XcmTransfer};
use primitives::{Balance, CurrencyId};
use sp_runtime::{
	offchain::storage_lock::BlockNumberProvider,
	traits::{Saturating, Zero},
	ArithmeticError, FixedPointNumber, Permill,
};
use sp_std::{cmp::min, convert::From, ops::Mul, prelude::*};

use xcm::v0::{ExecuteXcm, MultiLocation, Xcm};

pub use module::*;
pub use weights::WeightInfo;

#[frame_support::pallet]
pub mod module {
	use super::*;

	pub type RelaychainBlockNumberOf<T> = <<T as Config>::RelaychainBlockNumber as BlockNumberProvider>::BlockNumber;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;

		/// Multi-currency support for asset management
		type Currency: MultiReservableCurrency<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

		/// The Currency ID for the Staking asset
		#[pallet::constant]
		type StakingCurrencyId: Get<CurrencyId>;

		/// The Currency ID for the Liquid asset
		#[pallet::constant]
		type LiquidCurrencyId: Get<CurrencyId>;

		/// Origin represented Governance
		type GovernanceOrigin: EnsureOrigin<Self::Origin>;

		/// The minimal amount of Staking currency to be locked
		#[pallet::constant]
		type MinimumMintThreshold: Get<Balance>;

		/// The interface to Cross-chain transfer.
		type XcmTransfer: XcmTransfer<Self::AccountId, Balance, CurrencyId>;

		/// XCM executor.
		type XcmExecutor: ExecuteXcm<Self::Call>;

		/// The Call builder for communicating with Relaychain via XCM messaging.
		type RelaychainCallBuilder: CallBuilder<AccountId = Self::AccountId, Balance = Balance>;

		/// The sovereign sub-account for where the staking currencies are sent to.
		#[pallet::constant]
		type SovereignSubAccountLocation: Get<MultiLocation>;

		/// The default exchange rate for liquid currency to staking currency.
		#[pallet::constant]
		type DefaultExchangeRate: Get<ExchangeRate>;

		/// The maximum rewards that are earned on the relaychain.
		#[pallet::constant]
		type MaxRewardPerEra: Get<Permill>;

		/// The fixed cost of transaction fee for XCM transfers.
		#[pallet::constant]
		type MintFee: Get<Balance>;

		/// The fixed cost of withdrawing Staking currency via redeem.
		#[pallet::constant]
		type BaseWithdrawFee: Get<Permill>;

		/// Block number provider for the relaychain.
		type RelaychainBlockNumber: BlockNumberProvider<BlockNumber = Self::BlockNumber>;

		/// The account ID to redeem from on the relaychain.
		#[pallet::constant]
		type ParachainAccount: Get<Self::AccountId>;

		/// The maximum number of redeem requests to match in "Mint" extrinsic.
		#[pallet::constant]
		type MaximumRedeemRequestMatchesForMint: Get<u32>;

		/// Unbounding slashing spans for unbounding on the relaychain.
		#[pallet::constant]
		type RelaychainUnboundingSlashingSpans: Get<u32>;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The total amount for the Staking currency must be more than zero.
		InvalidTotalStakingCurrency,
		/// The mint amount is below the minimum threshold allowed.
		MintAmountBelowMinimumThreshold,
		/// The amount of Staking currency used has exceeded the cap allowed.
		ExceededStakingCurrencyMintCap,
		/// There isn't enough reserved currencies to cancel the redeem request.
		InsufficientReservedBalances,
		/// Amount redeemed is above total amount staked.
		InsufficientTotalStakingCurrency,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	#[pallet::metadata(T::AccountId = "AccountId", RelaychainBlockNumberOf<T> = "RelaychainBlockNumner")]
	pub enum Event<T: Config> {
		/// The user has Staked some currencies to mint Liquid Currency.
		/// \[user, amount_staked, amount_minted\]
		Minted(T::AccountId, Balance, Balance),

		/// The total amount of the staking currency on the relaychain has been
		/// set.\[total_staking_currency\]
		TotalStakingCurrencySet(Balance),

		/// The mint cap for Staking currency is updated.\[new_cap\]
		StakingCurrencyMintCapUpdated(Balance),

		/// A new weight for XCM transfers has been set.\[new_weight\]
		XcmDestWeightSet(Weight),

		/// The redeem request has been cancelled, and funds un-reserved.
		/// \[who, liquid_amount_unreserved\]
		RedeemRequestCancelled(T::AccountId, Balance),

		/// A new Redeem request has been registered.
		/// \[who, liquid_amount\]
		RedeemRequested(T::AccountId, Balance),

		/// The user has redeemed some Liquid currency back to Staking currency.
		/// \[user, staking_amount_redeemed, liquid_amount_deducted\]
		Redeemed(T::AccountId, Balance, Balance),

		/// A new Unbound request added to the schedule.
		/// \[staking_amount, relaychain_blocknumber\]
		ScheduledUnboundAdded(Balance, RelaychainBlockNumberOf<T>),

		/// The ScheduledUnbound has been replaced.
		ScheduledUnboundReplaced,
	}

	/// The total amount of the staking currency on the relaychain.
	/// This info is used to calculate the exchange rate between Staking and Liquid currencies.
	/// TotalStakingCurrency: value: Balance
	#[pallet::storage]
	#[pallet::getter(fn total_staking_currency)]
	pub type TotalStakingCurrency<T: Config> = StorageValue<_, Balance, ValueQuery>;

	/// The cap on the total amount of staking currency allowed to mint Liquid currency.
	/// StakingCurrencyMintCap: value: Balance
	#[pallet::storage]
	#[pallet::getter(fn staking_currency_mint_cap)]
	pub type StakingCurrencyMintCap<T: Config> = StorageValue<_, Balance, ValueQuery>;

	/// The extra weight for cross-chain XCM transfers.
	/// xcm_dest_weight: value: Weight
	#[pallet::storage]
	#[pallet::getter(fn xcm_dest_weight)]
	pub type XcmDestWeight<T: Config> = StorageValue<_, Weight, ValueQuery>;

	/// Requests to redeem staked currencies.
	/// RedeemRequests: Map: AccountId => Option<(liquid_amount: Balance, addtional_fee: Permill)>
	#[pallet::storage]
	#[pallet::getter(fn redeem_requests)]
	pub type RedeemRequests<T: Config> = StorageMap<_, Twox64Concat, T::AccountId, (Balance, Permill), OptionQuery>;

	/// The amount of staking currency that is available to be redeemed.
	/// AvailableStakingBalance: value: Balance
	#[pallet::storage]
	#[pallet::getter(fn available_staking_balance)]
	pub type AvailableStakingBalance<T: Config> = StorageValue<_, Balance, ValueQuery>;

	/// Funds that will be unbounded in the future
	/// ScheduledUnbound:
	#[pallet::storage]
	#[pallet::getter(fn scheduled_unbound)]
	pub type ScheduledUnbound<T: Config> = StorageValue<_, Vec<(Balance, RelaychainBlockNumberOf<T>)>, ValueQuery>;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {
		fn on_idle(_n: T::BlockNumber, remaining_weight: crate::weights::Weight) -> crate::weights::Weight {
			let required_weight = <T as Config>::WeightInfo::on_idle();
			let mut current_weight = 0;
			if remaining_weight > required_weight {
				let mut scheduled_unbound = Self::scheduled_unbound();
				if scheduled_unbound.is_empty() {
					return 0;
				}
				let (staking_amount, block_number) = scheduled_unbound[0];
				if T::RelaychainBlockNumber::current_block_number() >= block_number {
					let xcm_call = Self::construct_xcm_unreserve_message(
						T::ParachainAccount::get(),
						staking_amount,
						required_weight,
					);
					// make XCM call to trigger withdraw_unbound and transfer
					let origin_location = T::SovereignSubAccountLocation::get();
					let outcome = T::XcmExecutor::execute_xcm_in_credit(
						origin_location,
						xcm_call,
						required_weight,
						required_weight,
					);
					if outcome.ensure_complete().is_ok() {
						scheduled_unbound.remove(0);
						ScheduledUnbound::<T>::put(scheduled_unbound);

						let available_staking = Self::available_staking_balance()
							.checked_add(staking_amount)
							.expect("Total available staking currency cannot overflow.");
						AvailableStakingBalance::<T>::put(available_staking);

						current_weight = required_weight;
					}
				}
			}
			current_weight
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Mint some Liquid currency, by locking up the given amount of Staking currency.
		/// Will try to match Redeem Requests if available. Remaining amount is minted via XCM.
		///
		/// The exchange rate is calculated using the ratio of the total amount of the staking and
		/// liquid currency.
		///
		/// If any amount is minted through XCM, a portion of that amount (T::MintFee and
		/// T::MaxRewardPerEra) is reducted as fee.
		///
		/// Parameters:
		/// - `amount`: The amount of Staking currency to be exchanged.
		#[pallet::weight(< T as Config >::WeightInfo::mint())]
		#[transactional]
		pub fn mint(origin: OriginFor<T>, amount: Balance) -> DispatchResult {
			let minter = ensure_signed(origin)?;

			Self::do_mint_with_requests(&minter, amount, vec![])
		}

		/// Mint some Liquid currency, by locking up the given amount of Staking currency.
		/// This is similar with the mint() extrinsic, except that the given Redeem Requests are
		/// matched with priority.
		///
		/// Parameters:
		/// - `amount`: The amount of Staking currency to be exchanged.
		/// - `requests`: The redeem requests that are prioritized to match.
		#[pallet::weight(< T as Config >::WeightInfo::mint_for_requests())]
		#[transactional]
		pub fn mint_for_requests(origin: OriginFor<T>, amount: Balance, requests: Vec<T::AccountId>) -> DispatchResult {
			let minter = ensure_signed(origin)?;

			Self::do_mint_with_requests(&minter, amount, requests)
		}

		/// Sets the total amount of the Staking currency that are currently on the relaychain.
		/// Requires `T::GovernanceOrigin`
		///
		/// Parameters:
		/// - `staking_total`: The current amount of the Staking currency. Used to calculate
		///   conversion rate.
		#[pallet::weight(< T as Config >::WeightInfo::set_total_staking_currency())]
		#[transactional]
		pub fn set_total_staking_currency(origin: OriginFor<T>, staking_total: Balance) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;
			ensure!(!staking_total.is_zero(), Error::<T>::InvalidTotalStakingCurrency);

			TotalStakingCurrency::<T>::put(staking_total);
			Self::deposit_event(Event::<T>::TotalStakingCurrencySet(staking_total));

			Ok(())
		}

		/// Updates the cap for how much Staking currency can be used to Mint liquid currency.
		/// Requires `T::GovernanceOrigin`
		///
		/// Parameters:
		/// - `new_cap`: The new cap for staking currency.
		#[pallet::weight(< T as Config >::WeightInfo::set_minting_cap())]
		#[transactional]
		pub fn set_minting_cap(origin: OriginFor<T>, new_cap: Balance) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;

			StakingCurrencyMintCap::<T>::put(new_cap);
			Self::deposit_event(Event::<T>::StakingCurrencyMintCapUpdated(new_cap));
			Ok(())
		}

		/// Sets the xcm_dest_weight for XCM transfers.
		/// Requires `T::GovernanceOrigin`
		///
		/// Parameters:
		/// - `xcm_dest_weight`: The new weight for XCM transfers.
		#[pallet::weight(< T as Config >::WeightInfo::set_xcm_dest_weight())]
		#[transactional]
		pub fn set_xcm_dest_weight(origin: OriginFor<T>, xcm_dest_weight: Weight) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;

			XcmDestWeight::<T>::put(xcm_dest_weight);
			Self::deposit_event(Event::<T>::XcmDestWeightSet(xcm_dest_weight));
			Ok(())
		}

		/// Put in an request to redeem Staking currencies used to mint Liquid currency.
		/// The redemption will happen after the currencies are unbounded on the relaychain.
		///
		/// Parameters:
		/// - `liquid_amount`: The amount of liquid currency to be redeemed into Staking currency.
		/// - `additional_fee`: Percentage of the fee to be awarded to the minter.
		#[pallet::weight(< T as Config >::WeightInfo::request_redeem())]
		#[transactional]
		pub fn request_redeem(origin: OriginFor<T>, liquid_amount: Balance, additional_fee: Permill) -> DispatchResult {
			let who = ensure_signed(origin)?;

			if liquid_amount.is_zero() {
				// If the maount is zero, cancel previuos redeem request.
				if let Some((_, _)) = Self::redeem_requests(&who) {
					// Unreserve the liquid fee and remove the redeem request.
					RedeemRequests::<T>::remove(&who);

					Self::deposit_event(Event::<T>::RedeemRequestCancelled(who, liquid_amount));
				}
			} else {
				// Put in an redeem request, replaces the current request in storage.
				let actual_redeem_amount = min(
					liquid_amount,
					Self::convert_staking_to_liquid(Self::available_staking_balance())?,
				);
				if !actual_redeem_amount.is_zero() {
					// Reserve the liquid currencies to be redeem.
					T::Currency::reserve(T::LiquidCurrencyId::get(), &who, actual_redeem_amount)?;

					// override RedeemRequests
					RedeemRequests::<T>::insert(&who, (actual_redeem_amount, additional_fee));

					Self::deposit_event(Event::<T>::RedeemRequested(who, actual_redeem_amount));
				}
			}
			Ok(())
		}

		/// Request staking currencies to be unbounded from the Relaychain.
		///
		/// Requires `T::GovernanceOrigin`
		///
		/// Parameters:
		/// - `staking_amount`: The amount of staking currency to be unbounded.
		/// - `unbound_block`: The relaychain block number to unbound.
		#[pallet::weight(< T as Config >::WeightInfo::schedule_unbound())]
		#[transactional]
		pub fn schedule_unbound(
			origin: OriginFor<T>,
			staking_amount: Balance,
			unbound_block: RelaychainBlockNumberOf<T>,
		) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;

			ScheduledUnbound::<T>::append((staking_amount, unbound_block));

			Self::deposit_event(Event::<T>::ScheduledUnboundAdded(staking_amount, unbound_block));
			Ok(())
		}

		/// Replace the current storage for `ScheduledUnbound`.
		/// This should only be used to correct mistaken call of schedule_unbond or if something
		/// unexpected happened on relaychain.
		///
		/// Requires `T::GovernanceOrigin`
		///
		/// Parameters:
		/// - `new_unbounds`: The new ScheduledUnbound storage to replace the currrent storage.
		#[pallet::weight(< T as Config >::WeightInfo::replace_schedule_unbound())]
		#[transactional]
		pub fn replace_schedule_unbound(
			origin: OriginFor<T>,
			new_unbounds: Vec<(Balance, RelaychainBlockNumberOf<T>)>,
		) -> DispatchResult {
			T::GovernanceOrigin::ensure_origin(origin)?;

			ScheduledUnbound::<T>::put(new_unbounds);

			Self::deposit_event(Event::<T>::ScheduledUnboundReplaced);

			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Calculate the exchange rate between the Staking and Liquid currency.
		/// returns Ratio(liquid : staking) = liquid_total_issuance / total_staking_amount
		/// If the exchange rate cannot be calculated, T::DefaultExchangeRate is used
		fn get_exchange_rate() -> Ratio {
			let staking_total = Self::total_staking_currency();
			let liquid_total = T::Currency::total_issuance(T::LiquidCurrencyId::get());
			Ratio::checked_from_rational(liquid_total, staking_total).unwrap_or_else(T::DefaultExchangeRate::get)
		}

		/// Calculate the amount of Staking currency converted from Liquid currency.
		/// staking_amount = (1 / (total_staking_amount / liquid_total_issuance) * liquid_amount
		/// If the exchange rate cannot be calculated, T::DefaultExchangeRate is used
		fn convert_liquid_to_staking(liquid_amount: Balance) -> Result<Balance, DispatchError> {
			Self::get_exchange_rate()
				.reciprocal()
				.unwrap_or_else(T::DefaultExchangeRate::get)
				.checked_mul_int(liquid_amount)
				.ok_or(DispatchError::Arithmetic(ArithmeticError::Overflow))
		}

		/// Calculate the amount of Liquid currency converted from Staking currency.
		/// liquid_amount = (liquid_total_issuance / total_staking_amount) * staking_amount
		/// If the exchange rate cannot be calculated, T::DefaultExchangeRate is used
		fn convert_staking_to_liquid(staking_amount: Balance) -> Result<Balance, DispatchError> {
			Self::get_exchange_rate()
				.checked_mul_int(staking_amount)
				.ok_or(DispatchError::Arithmetic(ArithmeticError::Overflow))
		}

		/// Match a redeem request with a mint request. Attempt to redeem as much as possible.
		/// Transfer a reduced amount of Staking currency from the Minter to the Redeemer.
		/// Transfer the full amount of Liquid currency from Redeemer to Minter.
		/// Modify `liquid_amount_remaining` and store new RedeemRequest balances in `new_balances`.
		/// Deposit the "Redeemed" event.
		///
		/// NOTE: the `RedeemRequest` storage is NOT updated. New balance is pushed into
		/// `new_balances`, and should be processed after this.
		///
		/// Param:
		/// - `minter`: The AccountId requested the Mint
		/// - `redeemer`: The AccountId requested the Redeem
		/// - `request_amount`: The RedeemRequest's amount
		/// - `request_extra_fee`: The RedeemRequest's extra fee
		/// - `liquid_amount_remaining`: The amount of liquid currency still remain to be minted.
		///   Only redeem up to this amount.
		/// - `new_balances`: Stores the new `RedeemRequest` balances. This should be iterated after
		///   to update the actual storage in bulk. Actual `RedeemRequest` storage is NOT modified
		///   here.
		fn match_mint_with_redeem_request(
			minter: &T::AccountId,
			redeemer: &T::AccountId,
			request_amount: Balance,
			request_extra_fee: Permill,
			liquid_amount_remaining: &mut Balance,
			new_balances: &mut Vec<(T::AccountId, Balance, Permill)>,
		) -> DispatchResult {
			let liquid_amount_can_be_redeemed = min(request_amount, *liquid_amount_remaining);

			let new_amount = request_amount
				.checked_sub(liquid_amount_can_be_redeemed)
				.expect("min() guarantees that the amount deducted is less than the current balance; qed");
			*liquid_amount_remaining = liquid_amount_remaining
				.checked_sub(liquid_amount_can_be_redeemed)
				.expect("min() guarantees that the amount deducted is less than the total balance; qed");

			// Full amount of Liquid is transferred to the minter.
			let amount_repatriated = T::Currency::repatriate_reserved(
				T::LiquidCurrencyId::get(),
				redeemer,
				minter,
				liquid_amount_can_be_redeemed,
				BalanceStatus::Free,
			)?;
			ensure!(amount_repatriated.is_zero(), Error::<T>::InsufficientReservedBalances);

			// Fee is charged on the staking currency that is to be transferred.
			// staking_amount = original_staking_amount * ( 1 - base_with_fee - additional_fee )
			let mut staking_amount = Self::convert_liquid_to_staking(liquid_amount_can_be_redeemed)?;
			let fee_deducted_percentage = Permill::one()
				.saturating_sub(T::BaseWithdrawFee::get())
				.saturating_sub(request_extra_fee);
			staking_amount = fee_deducted_percentage.mul(staking_amount);

			// Transfer the reduced staking currency from Minter to Redeemer
			T::Currency::transfer(T::StakingCurrencyId::get(), minter, redeemer, staking_amount)?;

			new_balances.push((redeemer.clone(), new_amount, request_extra_fee));
			Self::deposit_event(Event::<T>::Redeemed(
				redeemer.clone(),
				staking_amount,
				liquid_amount_can_be_redeemed,
			));

			Ok(())
		}

		/// Mint some Liquid currency, by locking up the given amount of Staking currency.
		/// The redeem requests given in `requests` are prioritized to be matched. All other redeem
		/// requests are matched after. The remaining amount is minted through Staking on the
		/// Relaychain (via XCM).
		///
		/// Parameters:
		/// - `amount`: The amount of Staking currency to be exchanged.
		/// - `requests`: The redeem requests that are prioritized to match.
		fn do_mint_with_requests(
			minter: &T::AccountId,
			amount: Balance,
			requests: Vec<T::AccountId>,
		) -> DispatchResult {
			// Ensure the amount is above the minimum, after the MintFee is deducted.
			ensure!(
				amount > T::MinimumMintThreshold::get().saturating_add(T::MintFee::get()),
				Error::<T>::MintAmountBelowMinimumThreshold
			);

			let staking_currency = T::StakingCurrencyId::get();

			// ensure the user has enough funds on their account.
			T::Currency::ensure_can_withdraw(staking_currency, &minter, amount)?;

			// Attempt to match redeem requests if there are any.
			let total_liquid_to_mint = Self::convert_staking_to_liquid(amount)?;

			// The amount of liquid currency to be redeemed for the mint reuqest.
			let mut liquid_remaining = total_liquid_to_mint;

			// New balances after redeem requests are fullfilled.
			let mut new_balances: Vec<(T::AccountId, Balance, Permill)> = vec![];

			// Iterate through the prioritized requests first
			for redeemer in requests {
				// If all the currencies are minted, return.
				if liquid_remaining.is_zero() {
					break;
				}

				// Check if the redeem request exists
				if let Some((request_amount, extra_fee)) = Self::redeem_requests(&redeemer) {
					Self::match_mint_with_redeem_request(
						&minter,
						&redeemer,
						request_amount,
						extra_fee,
						&mut liquid_remaining,
						&mut new_balances,
					)?;
				}
			}

			// Update storage to the new balances. Remove Redeem requests that have been filled.
			for (redeemer, new_balance, extra_fee) in &new_balances {
				if new_balance.is_zero() {
					RedeemRequests::<T>::remove(&redeemer);
				} else {
					RedeemRequests::<T>::insert(&redeemer, (new_balance, extra_fee));
				}
			}
			// Redeem request storage has now been updated.
			new_balances.clear();

			let mut redeem_requests_limit_remaining = T::MaximumRedeemRequestMatchesForMint::get();
			// Iterate all remaining redeem requests now.
			for (redeemer, (request_amount, extra_fee)) in RedeemRequests::<T>::iter() {
				// If all the currencies are minted, return.
				if liquid_remaining.is_zero() || redeem_requests_limit_remaining.is_zero() {
					break;
				}
				Self::match_mint_with_redeem_request(
					&minter,
					&redeemer,
					request_amount,
					extra_fee,
					&mut liquid_remaining,
					&mut new_balances,
				)?;
				redeem_requests_limit_remaining -= 1;
			}

			// Update storage to the new balances. Remove Redeem requests that have been filled.
			for (redeemer, new_balance, extra_fee) in new_balances {
				if new_balance.is_zero() {
					RedeemRequests::<T>::remove(&redeemer);
				} else {
					RedeemRequests::<T>::insert(&redeemer, (new_balance, extra_fee));
				}
			}

			// If significant balance is left over, the remaining liquid currencies are minted through XCM.
			let mut staking_remaining = Self::convert_liquid_to_staking(liquid_remaining)?;
			if staking_remaining > T::MinimumMintThreshold::get().saturating_add(T::MintFee::get()) {
				// Calculate how much Liquid currency is to be minted.
				// liquid_to_mint = convert_to_liquid( (staked_amount - MintFee) * (1 - MaxRewardPerEra) )
				let mut liquid_to_mint = staking_remaining
					.checked_sub(T::MintFee::get())
					.expect("Mint amount is ensured to be greater than T::MintFee; qed");
				liquid_to_mint = (Permill::one().saturating_sub(T::MaxRewardPerEra::get())).mul(liquid_to_mint);
				liquid_to_mint = Self::convert_staking_to_liquid(liquid_to_mint)?;

				// Ensure the total amount staked doesn't exceed the cap.
				let new_total_staking_currency = Self::total_staking_currency()
					.checked_add(staking_remaining)
					.ok_or(ArithmeticError::Overflow)?;
				ensure!(
					new_total_staking_currency <= Self::staking_currency_mint_cap(),
					Error::<T>::ExceededStakingCurrencyMintCap
				);

				TotalStakingCurrency::<T>::put(new_total_staking_currency);

				// All checks pass. Proceed with Xcm transfer.
				T::XcmTransfer::transfer(
					minter.clone(),
					staking_currency,
					staking_remaining,
					T::SovereignSubAccountLocation::get(),
					Self::xcm_dest_weight(),
				)?;

				// Mint the liquid currency into the user's account.
				T::Currency::deposit(T::LiquidCurrencyId::get(), &minter, liquid_to_mint)?;

				staking_remaining = Balance::zero();
				liquid_remaining = liquid_remaining
					.checked_sub(liquid_to_mint)
					.expect("Liquid amount cannot be higher after fees are deducted; qed");
			}

			let actual_staked = amount
				.checked_sub(staking_remaining)
				.expect("Staking remaining cannot be more than the original; qed");
			let actual_liquid = total_liquid_to_mint
				.checked_sub(liquid_remaining)
				.expect("Liquid remaining cannot be more than the original; qed");

			Self::deposit_event(Event::<T>::Minted(minter.clone(), actual_staked, actual_liquid));

			Ok(())
		}

		/// Construct a XCM message
		fn construct_xcm_unreserve_message(
			parachain_account: T::AccountId,
			amount: Balance,
			weight_limit: u64,
		) -> Xcm<T::Call> {
			let xcm_message = T::RelaychainCallBuilder::utility_batch_call(vec![
				T::RelaychainCallBuilder::staking_withdraw_unbonded(T::RelaychainUnboundingSlashingSpans::get()),
				T::RelaychainCallBuilder::balances_transfer_keep_alive(parachain_account, amount),
			]);

			Xcm::Transact {
				origin_type: xcm::v0::OriginKind::SovereignAccount,
				require_weight_at_most: weight_limit,
				call: xcm_message.encode().into(),
			}
		}
	}
}
