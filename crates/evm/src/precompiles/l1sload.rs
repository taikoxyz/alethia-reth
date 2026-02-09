use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use alloy_primitives::{Address, B256, Bytes};
use reth_revm::precompile::{
    Precompile, PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult, u64_to_address,
};

pub const L1SLOAD: Precompile = Precompile::new(
    PrecompileId::Custom(std::borrow::Cow::Borrowed("L1SLOAD")),
    u64_to_address(0x10001),
    l1sload_run,
);

/// Gas constants for L1SLOAD precompile
pub const L1SLOAD_FIXED_GAS: u64 = 2000;
pub const L1SLOAD_PER_LOAD_GAS: u64 = 2000;

/// Expected input length: 20 bytes (address) + 32 bytes (storage key) + 32 bytes (block number),
/// total 84 bytes
pub const EXPECTED_INPUT_LENGTH: usize = 84;

/// Type alias for the L1 storage cache map.
type L1StorageCache = HashMap<(Address, B256, B256), B256>;

/// In-memory cache for L1 storage values
/// Key: (contract_address, storage_key, block_number) -> Value: storage_value
static L1_STORAGE_CACHE: LazyLock<Mutex<L1StorageCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Set a value in the L1 storage cache (keyed by address, storage key, and block number)
pub fn set_l1_storage_value(
    contract_address: Address,
    storage_key: B256,
    block_number: B256,
    value: B256,
) {
    if let Ok(mut cache) = L1_STORAGE_CACHE.lock() {
        cache.insert((contract_address, storage_key, block_number), value);
    }
}

/// Clear L1 storage cache
pub fn clear_l1_storage() {
    if let Ok(mut cache) = L1_STORAGE_CACHE.lock() {
        cache.clear();
    }
}

/// Get a value from the L1 storage cache
fn get_l1_storage_value(
    contract_address: Address,
    storage_key: B256,
    block_number: B256,
) -> Option<B256> {
    if let Ok(cache) = L1_STORAGE_CACHE.lock() {
        cache.get(&(contract_address, storage_key, block_number)).copied()
    } else {
        None
    }
}

/// L1SLOAD precompile - read storage values from L1 contracts (RIP-7728).
///
/// The input to the L1SLOAD precompile consists of:
/// - [0: 19]  (20 bytes)  - address: The L1 contract address
/// - [20: 51] (32 bytes)  - storageKey: The storage key to read
/// - [52: 83] (32 bytes)  - blockNumber: The L1 block number to read from
///
/// Output: Storage value (32 bytes)
pub fn l1sload_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    // Check gas limit
    let gas_used = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }

    // Validate input length
    if input.len() != EXPECTED_INPUT_LENGTH {
        return Err(PrecompileError::Other("Invalid input length".into()));
    }

    // Parse input parameters
    let contract_address = Address::from_slice(&input[0..20]);
    let storage_key = B256::from_slice(&input[20..52]);
    let block_number = B256::from_slice(&input[52..84]);

    // Get cached L1 storage value (keyed by address, storage key, and block number)
    let storage_value = get_l1_storage_value(contract_address, storage_key, block_number);

    match storage_value {
        Some(value) => {
            // Convert storage value to output bytes (32 bytes)
            let mut output = [0u8; 32];
            output.copy_from_slice(value.as_slice());
            Ok(PrecompileOutput::new(gas_used, Bytes::from(output)))
        }
        None => {
            // Return error if no cached data found
            Err(PrecompileError::Other("L1 storage value not found in cache".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use serial_test::serial;

    const TEST_ADDRESS: [u8; 20] = [1u8; 20];
    const TEST_STORAGE_KEY: [u8; 32] = [2u8; 32];
    const TEST_STORAGE_VALUE: [u8; 32] = [5u8; 32];
    const SUFFICIENT_GAS: u64 = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
    const INSUFFICIENT_GAS: u64 = SUFFICIENT_GAS - 1;

    /// Helper: create a B256 block number from a u64
    fn block_number_b256(n: u64) -> B256 {
        B256::from(U256::from(n))
    }

    /// Helper function to create test input (84 bytes) with given block number
    fn create_test_input(block_number: u64) -> Vec<u8> {
        let mut input = vec![0u8; EXPECTED_INPUT_LENGTH];
        input[0..20].copy_from_slice(&TEST_ADDRESS);
        input[20..52].copy_from_slice(&TEST_STORAGE_KEY);
        let bn_b256 = block_number_b256(block_number);
        input[52..84].copy_from_slice(bn_b256.as_slice());
        input
    }

    /// Helper function to setup storage cache with test data
    fn setup_test_storage(block: u64) -> (Address, B256, B256, B256) {
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block_num = block_number_b256(block);
        let value = B256::from(TEST_STORAGE_VALUE);

        set_l1_storage_value(address, key, block_num, value);
        (address, key, block_num, value)
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_invalid_input_lengths() {
        clear_l1_storage();

        // Test input too short (52 bytes - old format)
        let short_input = Bytes::from(vec![0u8; 52]);
        let result = l1sload_run(&short_input, SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject too short input");

        // Test input too long
        let long_input = Bytes::from(vec![0u8; 116]);
        let result = l1sload_run(&long_input, SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject too long input");
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_without_cached_storage() {
        clear_l1_storage();

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);

        assert!(result.is_err(), "Should fail when storage value is not cached");
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_cached_storage() {
        clear_l1_storage();

        let block = 100u64;
        let (_, _, _, expected_value) = setup_test_storage(block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(
            result.is_ok(),
            "Should succeed when storage value is cached. Error: {:?}",
            result.err()
        );

        let output = result.unwrap();

        // Verify gas usage
        let expected_gas = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
        assert_eq!(output.gas_used, expected_gas, "Gas usage should be fixed cost + per-load cost");

        // Verify output length
        assert_eq!(output.bytes.len(), 32, "Output should be exactly 32 bytes");

        // Verify output content matches stored value
        assert_eq!(
            output.bytes.as_ref(),
            &expected_value.0,
            "Output should match the cached storage value"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_with_insufficient_gas() {
        clear_l1_storage();

        let input = Bytes::from(create_test_input(100));
        let result = l1sload_run(&input, INSUFFICIENT_GAS);

        assert!(result.is_err(), "Should fail when gas limit is insufficient");
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_different_blocks() {
        clear_l1_storage();

        let block1 = 100u64;
        let block2 = 50u64; // arbitrary older block
        let (_, _, _, expected_value1) = setup_test_storage(block1);

        // Setup a second entry at a different block
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block_num2 = block_number_b256(block2);
        let value2 = B256::from([9u8; 32]);
        set_l1_storage_value(address, key, block_num2, value2);

        // Both blocks should return their respective values
        let result1 = l1sload_run(&Bytes::from(create_test_input(block1)), SUFFICIENT_GAS);
        assert!(result1.is_ok(), "Should succeed for block1");
        assert_eq!(result1.unwrap().bytes.as_ref(), &expected_value1.0);

        let result2 = l1sload_run(&Bytes::from(create_test_input(block2)), SUFFICIENT_GAS);
        assert!(result2.is_ok(), "Should succeed for block2");
        assert_eq!(result2.unwrap().bytes.as_ref(), &value2.0);
    }

    #[test]
    #[serial]
    fn test_storage_cache_operations() {
        clear_l1_storage();

        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block = block_number_b256(100);
        let value = B256::from(TEST_STORAGE_VALUE);

        // Verify cache is initially empty
        assert!(
            get_l1_storage_value(address, key, block).is_none(),
            "Cache should be empty initially"
        );

        set_l1_storage_value(address, key, block, value);
        assert_eq!(
            get_l1_storage_value(address, key, block),
            Some(value),
            "Should retrieve the cached value"
        );

        // Different block number should not have the value
        let other_block = block_number_b256(101);
        assert!(
            get_l1_storage_value(address, key, other_block).is_none(),
            "Different block number should not match"
        );
    }

    #[test]
    #[serial]
    fn test_cache_key_uniqueness() {
        clear_l1_storage();

        let address1 = Address::from([1u8; 20]);
        let address2 = Address::from([2u8; 20]);
        let key1 = B256::from([1u8; 32]);
        let key2 = B256::from([2u8; 32]);
        let block1 = block_number_b256(100);
        let block2 = block_number_b256(200);
        let value1 = B256::from([10u8; 32]);
        let value2 = B256::from([20u8; 32]);

        set_l1_storage_value(address1, key1, block1, value1);
        set_l1_storage_value(address2, key2, block2, value2);

        // Verify each key returns its correct value
        assert_eq!(get_l1_storage_value(address1, key1, block1), Some(value1));
        assert_eq!(get_l1_storage_value(address2, key2, block2), Some(value2));

        // Verify non-existent combinations return None
        assert!(get_l1_storage_value(address1, key2, block1).is_none());
        assert!(get_l1_storage_value(address2, key1, block2).is_none());
        assert!(get_l1_storage_value(address1, key1, block2).is_none());
    }
}
