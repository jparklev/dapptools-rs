use crate::{Evm, FAUCET_ACCOUNT};

use ethers::types::{Address, Bytes, U256};

use sputnik::{
    backend::{Backend, MemoryAccount},
    executor::{MemoryStackState, StackExecutor, StackState, StackSubstateMetadata},
    Config, CreateScheme, ExitReason, ExitRevert, Transfer,
};
use std::{collections::BTreeMap, marker::PhantomData};

use eyre::Result;

use super::SputnikExecutor;

pub type MemoryState = BTreeMap<Address, MemoryAccount>;

// TODO: Check if we can implement this as the base layer of an ethers-provider
// Middleware stack instead of doing RPC calls.
pub struct Executor<S, E> {
    pub executor: E,
    pub gas_limit: u64,
    marker: PhantomData<S>,
}

impl<S, E> Executor<S, E> {
    pub fn from_executor(executor: E, gas_limit: u64) -> Self {
        Self { executor, gas_limit, marker: PhantomData }
    }
}

// Concrete implementation over the in-memory backend without cheatcodes
impl<'a, B: Backend>
    Executor<MemoryStackState<'a, 'a, B>, StackExecutor<'a, MemoryStackState<'a, 'a, B>>>
{
    /// Given a gas limit, vm version, initial chain configuration and initial state
    // TOOD: See if we can make lifetimes better here
    pub fn new(gas_limit: u64, config: &'a Config, backend: &'a B) -> Self {
        // setup gasometer
        let metadata = StackSubstateMetadata::new(gas_limit, config);
        // setup state
        let state = MemoryStackState::new(metadata, backend);
        // setup executor
        let executor = StackExecutor::new_with_precompile(state, config, Default::default());

        Self { executor, gas_limit, marker: PhantomData }
    }
}

// Note regarding usage of Generic vs Associated Types in traits:
//
// We use StackState as a trait and not as an associated type because we want to
// allow the developer what the db type should be. Whereas for ReturnReason, we want it
// to be generic across implementations, but we don't want to make it a user-controlled generic.
impl<'a, S, E> Evm<S> for Executor<S, E>
where
    E: SputnikExecutor<S>,
    S: StackState<'a>,
{
    type ReturnReason = ExitReason;

    fn revert() -> Self::ReturnReason {
        ExitReason::Revert(ExitRevert::Reverted)
    }

    fn is_success(reason: &Self::ReturnReason) -> bool {
        matches!(reason, ExitReason::Succeed(_))
    }

    fn is_fail(reason: &Self::ReturnReason) -> bool {
        !Self::is_success(reason)
    }

    fn reset(&mut self, state: S) {
        let mut _state = self.executor.state_mut();
        *_state = state;
    }

    /// given an iterator of contract address to contract bytecode, initializes
    /// the state with the contract deployed at the specified address
    fn initialize_contracts<T: IntoIterator<Item = (Address, Bytes)>>(&mut self, contracts: T) {
        let state_ = self.executor.state_mut();
        contracts.into_iter().for_each(|(address, bytecode)| {
            state_.set_code(address, bytecode.to_vec());
        })
    }

    fn set_balance(&mut self, address: Address, balance: U256) {
        self.executor
            .state_mut()
            .transfer(Transfer { source: *FAUCET_ACCOUNT, target: address, value: balance })
            .expect("could not transfer funds")
    }

    fn state(&self) -> &S {
        self.executor.state()
    }

    /// Deploys the provided contract bytecode
    fn deploy(
        &mut self,
        from: Address,
        calldata: Bytes,
        value: U256,
    ) -> Result<(Address, ExitReason, u64, Vec<String>)> {
        let gas_before = self.executor.gas_left();

        // The account's created contract address is pre-computed by using the account's nonce
        // before it executes the contract deployment transaction.
        let address = self.executor.create_address(CreateScheme::Legacy { caller: from });
        let status =
            self.executor.transact_create(from, value, calldata.to_vec(), self.gas_limit, vec![]);

        // get the deployment logs
        let logs = self.executor.logs();
        // and clear them
        self.executor.clear_logs();

        let gas_after = self.executor.gas_left();
        let gas = gas_before.saturating_sub(gas_after).saturating_sub(21000.into());

        if Self::is_fail(&status) {
            tracing::trace!(?status, "failed");
            Err(eyre::eyre!("deployment reverted, reason: {:?}", status))
        } else {
            tracing::trace!(?status, ?address, ?gas, "success");
            Ok((address, status, gas.as_u64(), logs))
        }
    }

    /// Runs the selected function
    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
        _is_static: bool,
    ) -> Result<(Bytes, ExitReason, u64, Vec<String>)> {
        let gas_before = self.executor.gas_left();

        let (status, retdata) =
            self.executor.transact_call(from, to, value, calldata.to_vec(), self.gas_limit, vec![]);

        tracing::trace!(logs_before = ?self.executor.logs());

        let gas_after = self.executor.gas_left();
        let gas = gas_before.saturating_sub(gas_after).saturating_sub(21000.into());

        // get the logs
        let logs = self.executor.logs();
        tracing::trace!(logs_after = ?self.executor.logs());
        // clear them
        self.executor.clear_logs();

        Ok((retdata.into(), status, gas.as_u64(), logs))
    }
}

#[cfg(any(test, feature = "sputnik-helpers"))]
pub mod helpers {
    use super::*;
    use ethers::types::H160;
    use sputnik::backend::{MemoryBackend, MemoryVicinity};

    pub fn new_backend(vicinity: &MemoryVicinity, state: MemoryState) -> MemoryBackend<'_> {
        MemoryBackend::new(vicinity, state)
    }

    pub fn new_vicinity() -> MemoryVicinity {
        MemoryVicinity {
            gas_price: U256::zero(),
            origin: H160::default(),
            block_hashes: Vec::new(),
            block_number: Default::default(),
            block_coinbase: Default::default(),
            block_timestamp: Default::default(),
            block_difficulty: Default::default(),
            block_gas_limit: Default::default(),
            chain_id: U256::one(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        helpers::{new_backend, new_vicinity},
        *,
    };
    use crate::test_helpers::{can_call_vm_directly, solidity_unit_test, COMPILED};

    use ethers::utils::id;
    use sputnik::{ExitReason, ExitRevert, ExitSucceed};

    #[test]
    fn sputnik_can_call_vm_directly() {
        let cfg = Config::istanbul();
        let compiled = COMPILED.get("Greeter").expect("could not find contract");

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let evm = Executor::new(12_000_000, &cfg, &backend);
        can_call_vm_directly(evm, compiled);
    }

    #[test]
    fn sputnik_solidity_unit_test() {
        let cfg = Config::istanbul();

        let compiled = COMPILED.get("GreeterTest").expect("could not find contract");

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let evm = Executor::new(12_000_000, &cfg, &backend);
        solidity_unit_test(evm, compiled);
    }

    #[test]
    fn failing_with_no_reason_if_no_setup() {
        let cfg = Config::istanbul();

        let compiled = COMPILED.get("GreeterTest").expect("could not find contract");

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(12_000_000, &cfg, &backend);

        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode.clone(), 0.into()).unwrap();

        let (status, res) = evm.executor.transact_call(
            Address::zero(),
            addr,
            0.into(),
            id("testFailGreeting()").to_vec(),
            evm.gas_limit,
            vec![],
        );
        assert_eq!(status, ExitReason::Revert(ExitRevert::Reverted));
        assert!(res.is_empty());
    }

    #[test]
    fn failing_solidity_unit_test() {
        let cfg = Config::istanbul();

        let compiled = COMPILED.get("GreeterTest").expect("could not find contract");

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(12_000_000, &cfg, &backend);

        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode.clone(), 0.into()).unwrap();

        // call the setup function to deploy the contracts inside the test
        let status = evm.setup(addr).unwrap().0;
        assert_eq!(status, ExitReason::Succeed(ExitSucceed::Stopped));

        let err = evm
            .call::<(), _, _>(Address::zero(), addr, "testFailGreeting()", (), 0.into())
            .unwrap_err();
        let (reason, gas_used) = match err {
            crate::EvmError::Execution { reason, gas_used, .. } => (reason, gas_used),
            _ => panic!("unexpected error variant"),
        };
        assert_eq!(reason, "not equal to `hi`".to_string());
        assert_eq!(gas_used, 30330);
    }

    #[test]
    fn test_can_call_large_contract() {
        let cfg = Config::istanbul();

        use dapp_solc::SolcBuilder;

        let compiled = SolcBuilder::new("./testdata/LargeContract.sol", &[], &[])
            .unwrap()
            .build_all()
            .unwrap();
        let compiled = compiled.get("LargeContract").unwrap();

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(13_000_000, &cfg, &backend);

        let from = Address::random();
        let (addr, _, _, _) = evm.deploy(from, compiled.bytecode.clone(), 0.into()).unwrap();

        // makes a call to the contract
        let sig = ethers::utils::id("foo()").to_vec();
        let res = evm.call_raw(from, addr, sig.into(), 0.into(), true).unwrap();
        // the retdata cannot be empty
        assert!(!res.0.as_ref().is_empty());
        // the call must be successful
        assert!(matches!(res.1, ExitReason::Succeed(_)));
    }
}
