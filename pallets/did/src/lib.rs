// KILT Blockchain – https://botlabs.org
// Copyright (C) 2019-2022 BOTLabs GmbH

// The KILT Blockchain is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The KILT Blockchain is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

// If you feel like getting in touch with us, you can do so at info@botlabs.org

//! # DID Pallet
//!
//! Provides W3C-compliant DID functionalities. A DID identifier is derived from
//! a KILT address and must be verifiable, i.e., must be able to generate
//! digital signatures that can be verified starting from a raw payload, its
//! signature, and the signer identifier. Currently, the DID pallet supports the
//! following types of keys: Ed25519, Sr25519, and Ecdsa for signing keys, and
//! X25519 for encryption keys.
//!
//! - [`Config`]
//! - [`Call`]
//! - [`Origin`]
//! - [`Pallet`]
//!
//! ### Terminology
//!
//! Each DID identifier is mapped to a set of keys, which in KILT are used for
//! different purposes.
//!
//! - One **authentication key**: used to sign and authorise DID-management
//!   operations (e.g., the update of some keys or the deletion of the whole
//!   DID). This is required to always be present as otherwise the DID becomes
//!   unusable since no operation signature can be verified anymore.
//!
//! - Zero or more **key agreement keys**: used by other parties that want to
//!   interact with the DID subject to perform ECDH and encrypt information
//!   addressed to the DID subject.
//!
//! - Zero or one **delegation key**: used to sign and authorise the creation of
//!   new delegation nodes on the KILT blockchain. In case no delegation key is
//!   present, the DID subject cannot write new delegations on the KILT
//!   blockchain. For more info, check the [delegation
//!   pallet](../../delegation/).
//!
//! - Zero or one **attestation key**: used to sign and authorise the creation
//!   of new attested claims on the KILT blockchain. In case no attestation key
//!   is present, the DID subject cannot write new attested claims on the KILT
//!   blockchain. For more info, check the [attestation
//!   pallet](../../attestation/).
//!
//! - A set of **public keys**: includes at least the previous keys in addition
//!   to any past attestation key that has been rotated but not entirely
//!   revoked.
//!
//! - A set of **service endpoints**: pointing to the description of the
//!   services the DID subject exposes. For more information, check the W3C DID
//!   Core specification.
//!
//! - A **transaction counter**: acts as a nonce to avoid replay or signature
//!   forgery attacks. Each time a DID-signed transaction is executed, the
//!   counter is incremented.
//!
//! ## Assumptions
//!
//! - The maximum number of new key agreement keys that can be specified in a
//!   creation or update operation is bounded by `MaxNewKeyAgreementKeys`.
//! - After it is generated and signed by a client, a DID-authorised operation
//!   can be submitted for evaluation anytime between the time the operation is
//!   created and [MaxBlocksTxValidity] blocks after that. After this time has
//!   elapsed, the operation is considered invalid.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::unused_unit)]

pub mod default_weights;
pub mod did_details;
pub mod errors;
pub mod origin;
pub mod service_endpoints;

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;

#[cfg(test)]
mod mock;
#[cfg(any(feature = "runtime-benchmarks", test))]
mod mock_utils;
#[cfg(test)]
mod tests;

mod signature;
mod utils;

pub use crate::{
	default_weights::WeightInfo,
	did_details::{
		DeriveDidCallAuthorizationVerificationKeyRelationship, DeriveDidCallKeyRelationshipResult,
		DidAuthorizedCallOperationWithVerificationRelationship, DidSignature, DidVerificationKeyRelationship,
		RelationshipDeriveError,
	},
	origin::{DidRawOrigin, EnsureDidOrigin},
	pallet::*,
	signature::DidSignatureVerify,
};

use codec::Encode;
use frame_support::{
	dispatch::{DispatchResult, Dispatchable, GetDispatchInfo, PostDispatchInfo},
	ensure,
	storage::types::StorageMap,
	traits::{Get, OnUnbalanced, WithdrawReasons},
	Parameter,
};
use frame_system::ensure_signed;
use sp_runtime::{
	traits::{Saturating, Zero},
	SaturatedConversion,
};
use sp_std::{boxed::Box, fmt::Debug, prelude::Clone};

#[cfg(feature = "runtime-benchmarks")]
use frame_system::RawOrigin;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use crate::service_endpoints::utils as service_endpoints_utils;
	use frame_support::{
		pallet_prelude::*,
		traits::{Currency, ExistenceRequirement, Imbalance, ReservableCurrency, StorageVersion},
	};
	use frame_system::pallet_prelude::*;
	use kilt_support::{
		deposit::Deposit,
		traits::{CallSources, StorageDepositCollector},
	};
	use sp_runtime::traits::BadOrigin;

	use crate::{
		did_details::{
			DeriveDidCallAuthorizationVerificationKeyRelationship, DidAuthorizedCallOperation, DidCreationDetails,
			DidDetails, DidEncryptionKey, DidSignature, DidVerifiableIdentifier, DidVerificationKey,
			RelationshipDeriveError,
		},
		errors::{DidError, InputError, SignatureError, StorageError},
		service_endpoints::{DidEndpoint, ServiceEndpointId},
	};

	/// The current storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(4);

	/// Reference to a payload of data of variable size.
	pub type Payload = [u8];

	/// Type for a DID key identifier.
	pub type KeyIdOf<T> = <T as frame_system::Config>::Hash;

	/// Type for a DID subject identifier.
	pub type DidIdentifierOf<T> = <T as Config>::DidIdentifier;

	/// Type for a Kilt account identifier.
	pub type AccountIdOf<T> = <T as frame_system::Config>::AccountId;

	/// Type for a block number.
	pub type BlockNumberOf<T> = <T as frame_system::Config>::BlockNumber;

	/// Type for a runtime extrinsic callable under DID-based authorisation.
	pub type DidCallableOf<T> = <T as Config>::RuntimeCall;

	/// Type for origin that supports a DID sender.
	#[pallet::origin]
	pub type Origin<T> = DidRawOrigin<DidIdentifierOf<T>, AccountIdOf<T>>;

	pub type BalanceOf<T> = <CurrencyOf<T> as Currency<AccountIdOf<T>>>::Balance;
	pub(crate) type CurrencyOf<T> = <T as Config>::Currency;
	pub(crate) type NegativeImbalanceOf<T> = <<T as Config>::Currency as Currency<AccountIdOf<T>>>::NegativeImbalance;

	#[pallet::config]
	pub trait Config: frame_system::Config + Debug {
		/// Type for a dispatchable call that can be proxied through the DID
		/// pallet to support DID-based authorisation.
		type RuntimeCall: Parameter
			+ Dispatchable<PostInfo = PostDispatchInfo, RuntimeOrigin = <Self as Config>::RuntimeOrigin>
			+ GetDispatchInfo
			+ DeriveDidCallAuthorizationVerificationKeyRelationship;

		/// Type for a DID subject identifier.
		type DidIdentifier: Parameter + DidVerifiableIdentifier + MaxEncodedLen;

		/// Origin type expected by the proxied dispatchable calls.
		#[cfg(not(feature = "runtime-benchmarks"))]
		type RuntimeOrigin: From<DidRawOrigin<DidIdentifierOf<Self>, AccountIdOf<Self>>>;
		#[cfg(feature = "runtime-benchmarks")]
		type RuntimeOrigin: From<RawOrigin<DidIdentifierOf<Self>>>;

		/// The origin check for all DID calls inside this pallet.
		type EnsureOrigin: EnsureOrigin<
			<Self as frame_system::Config>::RuntimeOrigin,
			Success = <Self as Config>::OriginSuccess,
		>;

		/// The return type when the DID origin check was successful.
		type OriginSuccess: CallSources<AccountIdOf<Self>, DidIdentifierOf<Self>>;

		/// Overarching event type.
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The currency that is used to reserve funds for each did.
		type Currency: ReservableCurrency<AccountIdOf<Self>>;

		/// The amount of balance that will be taken for each DID as a deposit
		/// to incentivise fair use of the on chain storage. The deposit can be
		/// reclaimed when the DID is deleted.
		#[pallet::constant]
		type Deposit: Get<BalanceOf<Self>>;

		/// The amount of balance that will be taken for each DID as a fee to
		/// incentivise fair use of the on chain storage. The fee will not get
		/// refunded when the DID is deleted.
		#[pallet::constant]
		type Fee: Get<BalanceOf<Self>>;

		/// The logic for handling the fee.
		type FeeCollector: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// Maximum number of total public keys which can be stored per DID key
		/// identifier. This includes the ones currently used for
		/// authentication, key agreement, attestation, and delegation.
		#[pallet::constant]
		type MaxPublicKeysPerDid: Get<u32>;

		/// Maximum number of key agreement keys that can be added in a creation
		/// operation.
		#[pallet::constant]
		type MaxNewKeyAgreementKeys: Get<u32>;

		/// Maximum number of total key agreement keys that can be stored for a
		/// DID subject.
		///
		/// Should be greater than `MaxNewKeyAgreementKeys`.
		#[pallet::constant]
		type MaxTotalKeyAgreementKeys: Get<u32> + Debug + Clone + PartialEq;

		/// The maximum number of blocks a DID-authorized operation is
		/// considered valid after its creation.
		#[pallet::constant]
		type MaxBlocksTxValidity: Get<BlockNumberOf<Self>>;

		/// The maximum number of services that can be stored under a DID.
		#[pallet::constant]
		type MaxNumberOfServicesPerDid: Get<u32>;

		/// The maximum length of a service ID.
		#[pallet::constant]
		type MaxServiceIdLength: Get<u32>;

		/// The maximum length of a service type description.
		#[pallet::constant]
		type MaxServiceTypeLength: Get<u32>;

		/// The maximum number of a types description for a service endpoint.
		#[pallet::constant]
		type MaxNumberOfTypesPerService: Get<u32>;

		/// The maximum length of a service URL.
		#[pallet::constant]
		type MaxServiceUrlLength: Get<u32>;

		/// The maximum number of a URLs for a service endpoint.
		#[pallet::constant]
		type MaxNumberOfUrlsPerService: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	/// DIDs stored on chain.
	///
	/// It maps from a DID identifier to the DID details.
	#[pallet::storage]
	#[pallet::getter(fn get_did)]
	pub type Did<T> = StorageMap<_, Blake2_128Concat, DidIdentifierOf<T>, DidDetails<T>>;

	/// Service endpoints associated with DIDs.
	///
	/// It maps from (DID identifier, service ID) to the service details.
	#[pallet::storage]
	#[pallet::getter(fn get_service_endpoints)]
	pub type ServiceEndpoints<T> =
		StorageDoubleMap<_, Twox64Concat, DidIdentifierOf<T>, Blake2_128Concat, ServiceEndpointId<T>, DidEndpoint<T>>;

	/// Counter of service endpoints for each DID.
	///
	/// It maps from (DID identifier) to a 32-bit counter.
	#[pallet::storage]
	pub(crate) type DidEndpointsCount<T> = StorageMap<_, Blake2_128Concat, DidIdentifierOf<T>, u32, ValueQuery>;

	/// The set of DIDs that have been deleted and cannot therefore be created
	/// again for security reasons.
	///
	/// It maps from a DID identifier to a unit tuple, for the sake of tracking
	/// DID identifiers.
	#[pallet::storage]
	#[pallet::getter(fn get_deleted_did)]
	pub(crate) type DidBlacklist<T> = StorageMap<_, Blake2_128Concat, DidIdentifierOf<T>, ()>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new DID has been created.
		/// \[transaction signer, DID identifier\]
		DidCreated(AccountIdOf<T>, DidIdentifierOf<T>),
		/// A DID has been updated.
		/// \[DID identifier\]
		DidUpdated(DidIdentifierOf<T>),
		/// A DID has been deleted.
		/// \[DID identifier\]
		DidDeleted(DidIdentifierOf<T>),
		/// A DID-authorised call has been executed.
		/// \[DID caller, dispatch result\]
		DidCallDispatched(DidIdentifierOf<T>, DispatchResult),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The DID operation signature is not in the format the verification
		/// key expects.
		InvalidSignatureFormat,
		/// The DID operation signature is invalid for the payload and the
		/// verification key provided.
		InvalidSignature,
		/// The DID with the given identifier is already present on chain.
		DidAlreadyPresent,
		/// No DID with the given identifier is present on chain.
		DidNotPresent,
		/// One or more verification keys referenced are not stored in the set
		/// of verification keys.
		VerificationKeyNotPresent,
		/// The DID operation nonce is not equal to the current DID nonce + 1.
		InvalidNonce,
		/// The called extrinsic does not support DID authorisation.
		UnsupportedDidAuthorizationCall,
		/// The call had parameters that conflicted with each other
		/// or were invalid.
		InvalidDidAuthorizationCall,
		/// A number of new key agreement keys greater than the maximum allowed
		/// has been provided.
		MaxKeyAgreementKeysLimitExceeded,
		/// The maximum number of public keys for this DID key identifier has
		/// been reached.
		MaxPublicKeysPerDidExceeded,
		/// The maximum number of key agreements has been reached for the DID
		/// subject.
		MaxTotalKeyAgreementKeysExceeded,
		/// The DID call was submitted by the wrong account
		BadDidOrigin,
		/// The block number provided in a DID-authorized operation is invalid.
		TransactionExpired,
		/// The DID has already been previously deleted.
		DidAlreadyDeleted,
		/// Only the owner of the deposit can reclaim its reserved balance.
		NotOwnerOfDeposit,
		/// The origin is unable to reserve the deposit and pay the fee.
		UnableToPayFees,
		/// The maximum number of service endpoints for a DID has been exceeded.
		MaxNumberOfServicesPerDidExceeded,
		/// The service endpoint ID exceeded the maximum allowed length.
		MaxServiceIdLengthExceeded,
		/// One of the service endpoint types exceeded the maximum allowed
		/// length.
		MaxServiceTypeLengthExceeded,
		/// The maximum number of types for a service endpoint has been
		/// exceeded.
		MaxNumberOfTypesPerServiceExceeded,
		/// One of the service endpoint URLs exceeded the maximum allowed
		/// length.
		MaxServiceUrlLengthExceeded,
		/// The maximum number of URLs for a service endpoint has been exceeded.
		MaxNumberOfUrlsPerServiceExceeded,
		/// A service with the provided ID is already present for the given DID.
		ServiceAlreadyPresent,
		/// A service with the provided ID is not present under the given DID.
		ServiceNotPresent,
		/// One of the service endpoint details contains non-ASCII characters.
		InvalidServiceEncoding,
		/// The number of service endpoints stored under the DID is larger than
		/// the number of endpoints to delete.
		StoredEndpointsCountTooLarge,
		/// An error that is not supposed to take place, yet it happened.
		InternalError,
	}

	impl<T> From<DidError> for Error<T> {
		fn from(error: DidError) -> Self {
			match error {
				DidError::StorageError(storage_error) => Self::from(storage_error),
				DidError::SignatureError(operation_error) => Self::from(operation_error),
				DidError::InputError(input_error) => Self::from(input_error),
				DidError::InternalError => Self::InternalError,
			}
		}
	}

	impl<T> From<StorageError> for Error<T> {
		fn from(error: StorageError) -> Self {
			match error {
				StorageError::DidNotPresent => Self::DidNotPresent,
				StorageError::DidAlreadyPresent => Self::DidAlreadyPresent,
				StorageError::DidKeyNotPresent(_) | StorageError::KeyNotPresent => Self::VerificationKeyNotPresent,
				StorageError::MaxPublicKeysPerDidExceeded => Self::MaxPublicKeysPerDidExceeded,
				StorageError::MaxTotalKeyAgreementKeysExceeded => Self::MaxTotalKeyAgreementKeysExceeded,
				StorageError::DidAlreadyDeleted => Self::DidAlreadyDeleted,
			}
		}
	}

	impl<T> From<SignatureError> for Error<T> {
		fn from(error: SignatureError) -> Self {
			match error {
				SignatureError::InvalidSignature => Self::InvalidSignature,
				SignatureError::InvalidSignatureFormat => Self::InvalidSignatureFormat,
				SignatureError::InvalidNonce => Self::InvalidNonce,
				SignatureError::TransactionExpired => Self::TransactionExpired,
			}
		}
	}

	impl<T> From<InputError> for Error<T> {
		fn from(error: InputError) -> Self {
			match error {
				InputError::MaxKeyAgreementKeysLimitExceeded => Self::MaxKeyAgreementKeysLimitExceeded,
				InputError::MaxIdLengthExceeded => Self::MaxServiceIdLengthExceeded,
				InputError::MaxServicesCountExceeded => Self::MaxNumberOfServicesPerDidExceeded,
				InputError::MaxTypeCountExceeded => Self::MaxNumberOfTypesPerServiceExceeded,
				InputError::MaxTypeLengthExceeded => Self::MaxServiceTypeLengthExceeded,
				InputError::MaxUrlCountExceeded => Self::MaxNumberOfUrlsPerServiceExceeded,
				InputError::MaxUrlLengthExceeded => Self::MaxServiceUrlLengthExceeded,
				InputError::InvalidEncoding => Self::InvalidServiceEncoding,
			}
		}
	}

	impl<T> From<RelationshipDeriveError> for Error<T> {
		fn from(error: RelationshipDeriveError) -> Self {
			match error {
				RelationshipDeriveError::InvalidCallParameter => Self::InvalidDidAuthorizationCall,
				RelationshipDeriveError::NotCallableByDid => Self::UnsupportedDidAuthorizationCall,
			}
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Store a new DID on chain, after verifying that the creation
		/// operation has been signed by the KILT account associated with the
		/// identifier of the DID being created and that a DID with the same
		/// identifier has not previously existed on (and then deleted from) the
		/// chain.
		///
		/// There must be no DID information stored on chain under the same DID
		/// identifier.
		///
		/// The new keys added with this operation are stored under the DID
		/// identifier along with the block number in which the operation was
		/// executed.
		///
		/// The dispatch origin can be any KILT account with enough funds to
		/// execute the extrinsic and it does not have to be tied in any way to
		/// the KILT account identifying the DID subject.
		///
		/// Emits `DidCreated`.
		///
		/// # <weight>
		/// - The transaction's complexity is mainly dependent on the number of
		///   new key agreement keys and the number of new service endpoints
		///   included in the operation.
		/// ---------
		/// Weight: O(K) + O(N) where K is the number of new key agreement
		/// keys bounded by `MaxNewKeyAgreementKeys`, while N is the number of
		/// new service endpoints bounded by `MaxNumberOfServicesPerDid`.
		/// - Reads: [Origin Account], Did, DidBlacklist
		/// - Writes: Did (with K new key agreement keys), ServiceEndpoints
		///   (with N new service endpoints), DidEndpointsCount
		/// # </weight>
		#[pallet::weight({
			let new_key_agreement_keys = details.new_key_agreement_keys.len().saturated_into::<u32>();
			// We only consider the number of new endpoints.
			let new_services_count = details.new_service_details.len().saturated_into::<u32>();

			let ed25519_weight = <T as pallet::Config>::WeightInfo::create_ed25519_keys(
				new_key_agreement_keys,
				new_services_count,
			);
			let sr25519_weight = <T as pallet::Config>::WeightInfo::create_sr25519_keys(
				new_key_agreement_keys,
				new_services_count,
			);
			let ecdsa_weight = <T as pallet::Config>::WeightInfo::create_ecdsa_keys(
				new_key_agreement_keys,
				new_services_count,
			);

			ed25519_weight.max(sr25519_weight).max(ecdsa_weight)
		})]
		pub fn create(
			origin: OriginFor<T>,
			details: Box<DidCreationDetails<T>>,
			signature: DidSignature,
		) -> DispatchResult {
			let sender = ensure_signed(origin)?;

			ensure!(sender == details.submitter, BadOrigin);

			let did_identifier = details.did.clone();

			// Check the free balance before we do any heavy work.
			ensure!(
				<T::Currency as ReservableCurrency<AccountIdOf<T>>>::can_reserve(
					&sender,
					<T as Config>::Deposit::get() + <T as Config>::Fee::get()
				),
				Error::<T>::UnableToPayFees
			);

			// Make sure that DIDs cannot be created again after they have been deleted.
			ensure!(
				!DidBlacklist::<T>::contains_key(&did_identifier),
				Error::<T>::DidAlreadyDeleted
			);

			// There has to be no other DID with the same identifier already saved on chain,
			// otherwise generate a DidAlreadyPresent error.
			ensure!(!Did::<T>::contains_key(&did_identifier), Error::<T>::DidAlreadyPresent);

			let account_did_auth_key = did_identifier
				.verify_and_recover_signature(&details.encode(), &signature)
				.map_err(Error::<T>::from)?;

			// Validate all the size constraints for the service endpoints.
			let input_service_endpoints = details.new_service_details.clone();
			service_endpoints_utils::validate_new_service_endpoints(&input_service_endpoints)
				.map_err(Error::<T>::from)?;

			let did_entry =
				DidDetails::from_creation_details(*details, account_did_auth_key).map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			CurrencyOf::<T>::reserve(&did_entry.deposit.owner, did_entry.deposit.amount)?;

			// Withdraw the fee. We made sure that enough balance is available. But if this
			// fails, we don't withdraw anything.
			let imbalance = <T::Currency as Currency<AccountIdOf<T>>>::withdraw(
				&did_entry.deposit.owner,
				T::Fee::get(),
				WithdrawReasons::FEE,
				ExistenceRequirement::AllowDeath,
			)
			.unwrap_or_else(|_| NegativeImbalanceOf::<T>::zero());

			log::debug!("Creating DID {:?}", &did_identifier);
			T::FeeCollector::on_unbalanced(imbalance);

			Did::<T>::insert(&did_identifier, did_entry);
			input_service_endpoints.iter().for_each(|service| {
				ServiceEndpoints::<T>::insert(&did_identifier, &service.id, service.clone());
			});
			DidEndpointsCount::<T>::insert(&did_identifier, input_service_endpoints.len().saturated_into::<u32>());

			Self::deposit_event(Event::DidCreated(sender, did_identifier));

			Ok(())
		}

		/// Update the DID authentication key.
		///
		/// The old key is deleted from the set of public keys if it is
		/// not used in any other part of the DID. The new key is added to the
		/// set of public keys.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::set_ed25519_authentication_key().max(<T as pallet::Config>::WeightInfo::set_sr25519_authentication_key()).max(<T as pallet::Config>::WeightInfo::set_ecdsa_authentication_key()))]
		pub fn set_authentication_key(origin: OriginFor<T>, new_key: DidVerificationKey) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!(
				"Setting new authentication key {:?} for DID {:?}",
				&new_key,
				&did_subject
			);

			did_details
				.update_authentication_key(new_key, frame_system::Pallet::<T>::block_number())
				.map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Authentication key set");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Set or update the DID delegation key.
		///
		/// If an old key existed, it is deleted from the set of public keys if
		/// it is not used in any other part of the DID. The new key is added to
		/// the set of public keys.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::set_ed25519_delegation_key().max(<T as pallet::Config>::WeightInfo::set_sr25519_delegation_key()).max(<T as pallet::Config>::WeightInfo::set_ecdsa_delegation_key()))]
		pub fn set_delegation_key(origin: OriginFor<T>, new_key: DidVerificationKey) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!("Setting new delegation key {:?} for DID {:?}", &new_key, &did_subject);
			did_details
				.update_delegation_key(new_key, frame_system::Pallet::<T>::block_number())
				.map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Delegation key set");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Remove the DID delegation key.
		///
		/// The old key is deleted from the set of public keys if
		/// it is not used in any other part of the DID.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::remove_ed25519_delegation_key().max(<T as pallet::Config>::WeightInfo::remove_sr25519_delegation_key()).max(<T as pallet::Config>::WeightInfo::remove_ecdsa_delegation_key()))]
		pub fn remove_delegation_key(origin: OriginFor<T>) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!("Removing delegation key for DID {:?}", &did_subject);
			did_details.remove_delegation_key().map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Delegation key removed");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Set or update the DID attestation key.
		///
		/// If an old key existed, it is deleted from the set of public keys if
		/// it is not used in any other part of the DID. The new key is added to
		/// the set of public keys.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::set_ed25519_attestation_key().max(<T as pallet::Config>::WeightInfo::set_sr25519_attestation_key()).max(<T as pallet::Config>::WeightInfo::set_ecdsa_attestation_key()))]
		pub fn set_attestation_key(origin: OriginFor<T>, new_key: DidVerificationKey) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!("Setting new attestation key {:?} for DID {:?}", &new_key, &did_subject);
			did_details
				.update_attestation_key(new_key, frame_system::Pallet::<T>::block_number())
				.map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Attestation key set");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Remove the DID attestation key.
		///
		/// The old key is deleted from the set of public keys if
		/// it is not used in any other part of the DID.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::remove_ed25519_attestation_key().max(<T as pallet::Config>::WeightInfo::remove_sr25519_attestation_key()).max(<T as pallet::Config>::WeightInfo::remove_ecdsa_attestation_key()))]
		pub fn remove_attestation_key(origin: OriginFor<T>) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!("Removing attestation key for DID {:?}", &did_subject);
			did_details.remove_attestation_key().map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Attestation key removed");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Add a single new key agreement key to the DID.
		///
		/// The new key is added to the set of public keys.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::add_ed25519_key_agreement_key().max(<T as pallet::Config>::WeightInfo::add_sr25519_key_agreement_key()).max(<T as pallet::Config>::WeightInfo::add_ecdsa_key_agreement_key()))]
		pub fn add_key_agreement_key(origin: OriginFor<T>, new_key: DidEncryptionKey) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!("Adding new key agreement key {:?} for DID {:?}", &new_key, &did_subject);
			did_details
				.add_key_agreement_key(new_key, frame_system::Pallet::<T>::block_number())
				.map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Key agreement key set");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Remove a DID key agreement key from both its set of key agreement
		/// keys and as well as its public keys.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::remove_ed25519_key_agreement_key().max(<T as pallet::Config>::WeightInfo::remove_sr25519_key_agreement_key()).max(<T as pallet::Config>::WeightInfo::remove_ecdsa_key_agreement_key()))]
		pub fn remove_key_agreement_key(origin: OriginFor<T>, key_id: KeyIdOf<T>) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();
			let mut did_details = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			log::debug!("Removing key agreement key for DID {:?}", &did_subject);
			did_details.remove_key_agreement_key(key_id).map_err(Error::<T>::from)?;

			// *** No Fail beyond this call ***

			Did::<T>::insert(&did_subject, did_details);
			log::debug!("Key agreement key removed");

			Self::deposit_event(Event::DidUpdated(did_subject));
			Ok(())
		}

		/// Add a new service endpoint under the given DID.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did, ServiceEndpoints, DidEndpointsCount
		/// - Writes: Did, ServiceEndpoints, DidEndpointsCount
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::add_service_endpoint())]
		pub fn add_service_endpoint(origin: OriginFor<T>, service_endpoint: DidEndpoint<T>) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();

			service_endpoint
				.validate_against_constraints()
				.map_err(Error::<T>::from)?;

			// Verify that the DID is present.
			ensure!(Did::<T>::get(&did_subject).is_some(), Error::<T>::DidNotPresent);

			let currently_stored_endpoints_count = DidEndpointsCount::<T>::get(&did_subject);

			// Verify that there are less than the maximum limit of services stored.
			ensure!(
				currently_stored_endpoints_count < T::MaxNumberOfServicesPerDid::get(),
				Error::<T>::MaxNumberOfServicesPerDidExceeded
			);

			// *** No Fail after the following storage write ***

			ServiceEndpoints::<T>::try_mutate(
				&did_subject,
				service_endpoint.id.clone(),
				|existing_service| -> Result<(), Error<T>> {
					ensure!(existing_service.is_none(), Error::<T>::ServiceAlreadyPresent);
					*existing_service = Some(service_endpoint);
					Ok(())
				},
			)?;
			DidEndpointsCount::<T>::insert(&did_subject, currently_stored_endpoints_count.saturating_add(1));

			Self::deposit_event(Event::DidUpdated(did_subject));

			Ok(())
		}

		/// Remove the service with the provided ID from the DID.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidUpdated`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], ServiceEndpoints, DidEndpointsCount
		/// - Writes: Did, ServiceEndpoints, DidEndpointsCount
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::remove_service_endpoint())]
		pub fn remove_service_endpoint(origin: OriginFor<T>, service_id: ServiceEndpointId<T>) -> DispatchResult {
			let did_subject = T::EnsureOrigin::ensure_origin(origin)?.subject();

			// *** No Fail after the next call succeeds ***

			ensure!(
				ServiceEndpoints::<T>::take(&did_subject, &service_id).is_some(),
				Error::<T>::ServiceNotPresent
			);

			// Decrease the endpoints counter or delete the entry if it reaches 0.
			DidEndpointsCount::<T>::mutate_exists(&did_subject, |existing_endpoint_count| {
				let new_value = existing_endpoint_count.unwrap_or_default().saturating_sub(1);
				if new_value.is_zero() {
					*existing_endpoint_count = None;
				} else {
					*existing_endpoint_count = Some(new_value);
				}
			});

			Self::deposit_event(Event::DidUpdated(did_subject));

			Ok(())
		}

		/// Delete a DID from the chain and all information associated with it,
		/// after verifying that the delete operation has been signed by the DID
		/// subject using the authentication key currently stored on chain.
		///
		/// The referenced DID identifier must be present on chain before the
		/// delete operation is evaluated.
		///
		/// After it is deleted, a DID with the same identifier cannot be
		/// re-created ever again.
		///
		/// As the result of the deletion, all traces of the DID are removed
		/// from the storage, which results in the invalidation of all
		/// attestations issued by the DID subject.
		///
		/// The dispatch origin must be a DID origin proxied via the
		/// `submit_did_call` extrinsic.
		///
		/// Emits `DidDeleted`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Kills: Did entry associated to the DID identifier
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::delete(*endpoints_to_remove))]
		pub fn delete(origin: OriginFor<T>, endpoints_to_remove: u32) -> DispatchResult {
			let source = T::EnsureOrigin::ensure_origin(origin)?;
			let did_subject = source.subject();

			Pallet::<T>::delete_did(did_subject, endpoints_to_remove)
		}

		/// Reclaim a deposit for a DID. This will delete the DID and all
		/// information associated with it, after verifying that the caller is
		/// the owner of the deposit.
		///
		/// The referenced DID identifier must be present on chain before the
		/// delete operation is evaluated.
		///
		/// After it is deleted, a DID with the same identifier cannot be
		/// re-created ever again.
		///
		/// As the result of the deletion, all traces of the DID are removed
		/// from the storage, which results in the invalidation of all
		/// attestations issued by the DID subject.
		///
		/// Emits `DidDeleted`.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: [Origin Account], Did
		/// - Kills: Did entry associated to the DID identifier
		/// # </weight>
		#[pallet::weight(<T as pallet::Config>::WeightInfo::reclaim_deposit(*endpoints_to_remove))]
		pub fn reclaim_deposit(
			origin: OriginFor<T>,
			did_subject: DidIdentifierOf<T>,
			endpoints_to_remove: u32,
		) -> DispatchResult {
			let source = ensure_signed(origin)?;
			let did_entry = Did::<T>::get(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			ensure!(did_entry.deposit.owner == source, Error::<T>::NotOwnerOfDeposit);

			Pallet::<T>::delete_did(did_subject, endpoints_to_remove)
		}

		/// Proxy a dispatchable call of another runtime extrinsic that
		/// supports a DID origin.
		///
		/// The referenced DID identifier must be present on chain before the
		/// operation is dispatched.
		///
		/// A call submitted through this extrinsic must be signed with the
		/// right DID key, depending on the call. This information is provided
		/// by the `DidAuthorizedCallOperation` parameter, which specifies the
		/// DID subject acting as the origin of the call, the DID's tx counter
		/// (nonce), the dispatchable to call in case signature verification
		/// succeeds, the type of DID key to use to verify the operation
		/// signature, and the block number the operation was targeting for
		/// inclusion, when it was created and signed.
		///
		/// In case the signature is incorrect, the nonce is not valid, the
		/// required key is not present for the specified DID, or the block
		/// specified is too old the verification fails and the call is not
		/// dispatched. Otherwise, the call is properly dispatched with a
		/// `DidOrigin` origin indicating the DID subject.
		///
		/// A successful dispatch operation results in the tx counter associated
		/// with the given DID to be incremented, to mitigate replay attacks.
		///
		/// The dispatch origin can be any KILT account with enough funds to
		/// execute the extrinsic and it does not have to be tied in any way to
		/// the KILT account identifying the DID subject.
		///
		/// Emits `DidCallDispatched`.
		///
		/// # <weight>
		/// Weight: O(1) + weight of the dispatched call
		/// - Reads: [Origin Account], Did
		/// - Writes: Did
		/// # </weight>
		#[allow(clippy::boxed_local)]
		#[pallet::weight({
			let di = did_call.call.get_dispatch_info();
			let max_sig_weight = <T as pallet::Config>::WeightInfo::submit_did_call_ed25519_key()
			.max(<T as pallet::Config>::WeightInfo::submit_did_call_sr25519_key())
			.max(<T as pallet::Config>::WeightInfo::submit_did_call_ecdsa_key());

			(max_sig_weight.saturating_add(di.weight), di.class)
		})]
		pub fn submit_did_call(
			origin: OriginFor<T>,
			did_call: Box<DidAuthorizedCallOperation<T>>,
			signature: DidSignature,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			ensure!(did_call.submitter == who, Error::<T>::BadDidOrigin);

			let did_identifier = did_call.did.clone();

			// Compute the right DID verification key to use to verify the operation
			// signature
			let verification_key_relationship = did_call
				.call
				.derive_verification_key_relationship()
				.map_err(Error::<T>::from)?;

			// Wrap the operation in the expected structure, specifying the key retrieved
			let wrapped_operation = DidAuthorizedCallOperationWithVerificationRelationship {
				operation: *did_call,
				verification_key_relationship,
			};

			Self::verify_did_operation_signature_and_increase_nonce(&wrapped_operation, &signature)
				.map_err(Error::<T>::from)?;

			log::debug!("Dispatch call from DID {:?}", did_identifier);

			// Dispatch the referenced [Call] instance and return its result
			let DidAuthorizedCallOperation { did, call, .. } = wrapped_operation.operation;

			// *** No Fail beyond this point ***

			#[cfg(not(feature = "runtime-benchmarks"))]
			let result = call.dispatch(
				DidRawOrigin {
					id: did,
					submitter: who,
				}
				.into(),
			);
			#[cfg(feature = "runtime-benchmarks")]
			let result = call.dispatch(RawOrigin::Signed(did).into());

			let dispatch_event_payload = result.map(|_| ()).map_err(|e| e.error);

			Self::deposit_event(Event::DidCallDispatched(did_identifier, dispatch_event_payload));

			result
		}

		/// Changes the deposit owner.
		///
		/// The balance that is reserved by the current deposit owner will be
		/// freed and balance of the new deposit owner will get reserved.
		///
		/// The subject of the call must be the did owner.
		/// The sender of the call will be the new deposit owner.
		#[pallet::weight(<T as pallet::Config>::WeightInfo::change_deposit_owner())]
		pub fn change_deposit_owner(origin: OriginFor<T>) -> DispatchResult {
			let source = <T as Config>::EnsureOrigin::ensure_origin(origin)?;
			let subject = source.subject();
			let sender = source.sender();

			DidDepositCollector::<T>::change_deposit_owner(&subject, sender)?;

			Ok(())
		}

		/// Updates the deposit amount to the current deposit rate.
		///
		/// The sender must be the deposit owner.
		#[pallet::weight(<T as pallet::Config>::WeightInfo::update_deposit())]
		pub fn update_deposit(origin: OriginFor<T>, did: DidIdentifierOf<T>) -> DispatchResult {
			let sender = ensure_signed(origin)?;

			let did_entry = Did::<T>::get(&did).ok_or(Error::<T>::DidNotPresent)?;
			ensure!(did_entry.deposit.owner == sender, Error::<T>::BadDidOrigin);

			DidDepositCollector::<T>::update_deposit(&did)?;

			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Verify the validity (i.e., nonce, signature and mortality) of a
		/// DID-authorized operation and, if valid, update the DID state with
		/// the latest nonce.
		///
		/// # <weight>
		/// Weight: O(1)
		/// - Reads: Did
		/// - Writes: Did
		/// # </weight>
		pub fn verify_did_operation_signature_and_increase_nonce(
			operation: &DidAuthorizedCallOperationWithVerificationRelationship<T>,
			signature: &DidSignature,
		) -> Result<(), DidError> {
			// Check that the tx has not expired.
			Self::validate_block_number_value(operation.block_number)?;

			let mut did_details =
				Did::<T>::get(&operation.did).ok_or(DidError::StorageError(StorageError::DidNotPresent))?;

			Self::validate_counter_value(operation.tx_counter, &did_details)?;
			// Increase the tx counter as soon as it is considered valid, no matter if the
			// signature is valid or not.
			did_details.increase_tx_counter();
			Self::verify_payload_signature_with_did_key_type(
				&operation.encode(),
				signature,
				&did_details,
				operation.verification_key_relationship,
			)?;

			Did::<T>::insert(&operation.did, did_details);

			Ok(())
		}

		/// Check if the provided block number is valid,
		/// i.e., if the current blockchain block is in the inclusive range
		/// [operation_block_number, operation_block_number +
		/// MaxBlocksTxValidity].
		fn validate_block_number_value(block_number: BlockNumberOf<T>) -> Result<(), DidError> {
			let current_block_number = frame_system::Pallet::<T>::block_number();
			let allowed_range = block_number..=block_number.saturating_add(T::MaxBlocksTxValidity::get());

			ensure!(
				allowed_range.contains(&current_block_number),
				DidError::SignatureError(SignatureError::TransactionExpired)
			);

			Ok(())
		}

		/// Verify the validity of a DID-authorized operation nonce.
		/// To be valid, the nonce must be equal to the one currently stored +
		/// 1. This is to avoid quickly "consuming" all the possible values for
		/// the counter, as that would result in the DID being unusable, since
		/// we do not have yet any mechanism in place to wrap the counter value
		/// around when the limit is reached.
		fn validate_counter_value(counter: u64, did_details: &DidDetails<T>) -> Result<(), DidError> {
			// Verify that the operation counter is equal to the stored one + 1,
			// possibly wrapping around when u64::MAX is reached.
			let expected_nonce_value = did_details.last_tx_counter.wrapping_add(1);
			ensure!(
				counter == expected_nonce_value,
				DidError::SignatureError(SignatureError::InvalidNonce)
			);

			Ok(())
		}

		/// Verify a generic payload signature using a given DID verification
		/// key type.
		pub fn verify_payload_signature_with_did_key_type(
			payload: &Payload,
			signature: &DidSignature,
			did_details: &DidDetails<T>,
			key_type: DidVerificationKeyRelationship,
		) -> Result<(), DidError> {
			// Retrieve the needed verification key from the DID details, or generate an
			// error if there is no key of the type required
			let verification_key = did_details
				.get_verification_key_for_key_type(key_type)
				.ok_or(DidError::StorageError(StorageError::DidKeyNotPresent(key_type)))?;

			// Verify that the signature matches the expected format, otherwise generate
			// an error
			verification_key
				.verify_signature(payload, signature)
				.map_err(DidError::SignatureError)
		}

		/// Deletes DID details from storage, including its linked service
		/// endpoints, adds the identifier to the blacklisted DIDs and frees the
		/// deposit.
		fn delete_did(did_subject: DidIdentifierOf<T>, endpoints_to_remove: u32) -> DispatchResult {
			let current_endpoints_count = DidEndpointsCount::<T>::get(&did_subject);
			ensure!(
				current_endpoints_count <= endpoints_to_remove,
				Error::<T>::StoredEndpointsCountTooLarge
			);

			// *** No Fail beyond this point ***

			// This one can fail, albeit this should **never** be the case as we check for
			// the preconditions above.
			// If some items are remaining (e.g. a continuation cursor exists), it means
			// that there were more than the counter stored in `DidEndpointsCount`, and that
			// should never happen.
			if ServiceEndpoints::<T>::clear_prefix(&did_subject, current_endpoints_count, None)
				.maybe_cursor
				.is_some()
			{
				return Err(Error::<T>::InternalError.into());
			};

			// `take` calls `kill` internally
			let did_entry = Did::<T>::take(&did_subject).ok_or(Error::<T>::DidNotPresent)?;

			DidEndpointsCount::<T>::remove(&did_subject);
			kilt_support::free_deposit::<AccountIdOf<T>, CurrencyOf<T>>(&did_entry.deposit);
			// Mark as deleted to prevent potential replay-attacks of re-adding a previously
			// deleted DID.
			DidBlacklist::<T>::insert(&did_subject, ());

			log::debug!("Deleting DID {:?}", did_subject);

			Self::deposit_event(Event::DidDeleted(did_subject));

			Ok(())
		}
	}

	struct DidDepositCollector<T: Config>(PhantomData<T>);
	impl<T: Config> StorageDepositCollector<AccountIdOf<T>, DidIdentifierOf<T>> for DidDepositCollector<T> {
		type Currency = T::Currency;

		fn deposit(
			key: &DidIdentifierOf<T>,
		) -> Result<Deposit<AccountIdOf<T>, <Self::Currency as Currency<AccountIdOf<T>>>::Balance>, DispatchError> {
			let did_entry = Did::<T>::get(key).ok_or(Error::<T>::DidNotPresent)?;
			Ok(did_entry.deposit)
		}

		fn deposit_amount(_key: &DidIdentifierOf<T>) -> <Self::Currency as Currency<AccountIdOf<T>>>::Balance {
			T::Deposit::get()
		}

		fn store_deposit(
			key: &DidIdentifierOf<T>,
			deposit: Deposit<AccountIdOf<T>, <Self::Currency as Currency<AccountIdOf<T>>>::Balance>,
		) -> Result<(), DispatchError> {
			let did_entry = Did::<T>::get(key).ok_or(Error::<T>::DidNotPresent)?;
			Did::<T>::insert(key, DidDetails { deposit, ..did_entry });

			Ok(())
		}
	}
}
