//! Fixed Unzen zk gas schedule constants copied from the approved protocol spec.
//!
//! Source:
//! <https://github.com/taikoxyz/taiko-mono/blob/main/packages/protocol/docs/zk_gas_spec.md>

use alloy_primitives::{Address, address};

use super::schedule::{FAILSAFE_MULTIPLIER, SpawnEstimates, ZkGasSchedule};

/// Maximum zk gas permitted within a single Unzen block on Devnet, Hoodi, and Mainnet.
pub const BLOCK_ZK_GAS_LIMIT: u64 = 100_000_000;

/// Fixed zk gas charged once per block transaction on Devnet, Hoodi, and Mainnet Unzen.
///
/// Value sourced from <https://github.com/taikoxyz/taiko-mono/pull/21669>; covers the proving
/// cost of per-transaction sender ecrecovery.
pub const TX_INTRINSIC_ZK_GAS: u64 = 243_000;

/// Builds an Unzen-shaped zk gas schedule from the requested block limit, per-transaction
/// intrinsic charge, and opcode/precompile multiplier tables. Spawn estimates are identical
/// across all networks.
const fn unzen_schedule_with(
    block_limit: u64,
    tx_intrinsic_zk_gas: u64,
    opcode_multipliers: [u16; 256],
    precompile_multipliers: &'static [(Address, u16)],
) -> ZkGasSchedule {
    ZkGasSchedule {
        block_limit,
        tx_intrinsic_zk_gas,
        opcode_multipliers,
        precompile_multipliers,
        spawn_estimates: SpawnEstimates {
            call: 12_500,
            callcode: 12_500,
            delegatecall: 3_500,
            staticcall: 3_500,
            create: 37_000,
            create2: 44_500,
        },
    }
}

/// Default Unzen zk gas schedule used by Devnet, Hoodi, and Mainnet, with the recalibrated
/// multipliers.
pub static UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule = unzen_schedule_with(
    BLOCK_ZK_GAS_LIMIT,
    TX_INTRINSIC_ZK_GAS,
    unzen_opcode_multipliers(),
    UNZEN_PRECOMPILE_MULTIPLIERS,
);

/// Recalibrated Unzen precompile multipliers (Devnet / Hoodi / Mainnet), keyed by full address.
/// Precompiles not listed here fall back to [`FAILSAFE_MULTIPLIER`]. The canonical EVM precompiles
/// set only their last byte (`0x…01` through `0x…11`), so [`Address::with_last_byte`] spells their
/// keys; p256verify (RIP-7212) is at `0x100`, whose second-to-last byte is non-zero, so it needs a
/// full-address literal.
const UNZEN_PRECOMPILE_MULTIPLIERS: &[(Address, u16)] = &[
    (Address::with_last_byte(0x01), 47),  // ecrecover
    (Address::with_last_byte(0x02), 10),  // sha256
    (Address::with_last_byte(0x03), 4),   // ripemd160
    (Address::with_last_byte(0x04), 6),   // identity
    (Address::with_last_byte(0x05), 154), // modexp
    (Address::with_last_byte(0x06), 19),  // bn128_add
    (Address::with_last_byte(0x07), 58),  // bn128_mul
    (Address::with_last_byte(0x08), 54),  // bn128_pairing
    (Address::with_last_byte(0x09), 166), // blake2f
    (Address::with_last_byte(0x0a), 859), // point_evaluation
    (Address::with_last_byte(0x0b), 201), // bls12_g1add
    (Address::with_last_byte(0x0c), 93),  // bls12_g1msm
    (Address::with_last_byte(0x0d), 230), // bls12_g2add
    (Address::with_last_byte(0x0e), 71),  // bls12_g2msm
    (Address::with_last_byte(0x0f), 365), // bls12_pairing
    (Address::with_last_byte(0x10), 246), // bls12_map_fp_to_g1
    (Address::with_last_byte(0x11), 208), // bls12_map_fp2_to_g2
    // p256verify (RIP-7212) at 0x0000…0100 — outside the canonical 0x..XX range, so it
    // needs a full-address literal. Multiplier from taikoxyz/taiko-mono#21748.
    (address!("0x0000000000000000000000000000000000000100"), 163),
    // TODO(calibrate): 50 is a placeholder. Without an explicit entry these fall through to
    // FAILSAFE_MULTIPLIER (u16::MAX), which exceeds the 100M block budget on a single call.
    // Replace with measured proving-cycles-per-native-gas (cf. `taikoxyz/taiko-mono#21748`
    // for p256verify) before shipping outside devnet, and update `MASAYA_…` below.
    (address!("0x0000000000000000000000000000000000010001"), 50), // L1SLOAD     — TODO(calibrate)
    (address!("0x0000000000000000000000000000000000010002"), 50), // L1STATICCALL — TODO(calibrate)
];

/// Returns the recalibrated Unzen opcode multiplier table, with fail-safe defaults for unlisted
/// opcodes.
const fn unzen_opcode_multipliers() -> [u16; 256] {
    let mut array = [FAILSAFE_MULTIPLIER; 256];
    array[0x00] = 0; // stop
    array[0x01] = 19; // add
    array[0x02] = 19; // mul
    array[0x03] = 22; // sub
    array[0x04] = 76; // div
    array[0x05] = 78; // sdiv
    array[0x06] = 66; // mod
    array[0x07] = 28; // smod
    array[0x08] = 52; // addmod
    array[0x09] = 113; // mulmod
    array[0x0a] = 21; // exp
    array[0x0b] = 17; // signextend
    array[0x10] = 19; // lt
    array[0x11] = 19; // gt
    array[0x12] = 20; // slt
    array[0x13] = 19; // sgt
    array[0x14] = 36; // eq
    array[0x15] = 16; // iszero
    array[0x16] = 19; // and
    array[0x17] = 20; // or
    array[0x18] = 18; // xor
    array[0x19] = 15; // not
    array[0x1a] = 17; // byte
    array[0x1b] = 24; // shl
    array[0x1c] = 22; // shr
    array[0x1d] = 21; // sar
    array[0x1e] = 14; // clz
    array[0x20] = 31; // keccak256
    array[0x30] = 19; // address
    array[0x31] = 4; // balance
    array[0x32] = 21; // origin
    array[0x33] = 18; // caller
    array[0x34] = 11; // callvalue
    array[0x35] = 22; // calldataload
    array[0x36] = 13; // calldatasize
    array[0x37] = 13; // calldatacopy
    array[0x38] = 11; // codesize
    array[0x39] = 12; // codecopy
    array[0x3a] = 15; // gasprice
    array[0x3b] = 4; // extcodesize
    array[0x3c] = 4; // extcodecopy
    array[0x3d] = 12; // returndatasize
    array[0x3e] = 10; // returndatacopy
    array[0x3f] = 7; // extcodehash
    array[0x40] = 6; // blockhash
    array[0x41] = 18; // coinbase
    array[0x42] = 10; // timestamp
    array[0x43] = 12; // number
    array[0x44] = 42; // prevrandao
    array[0x45] = 13; // gaslimit
    array[0x46] = 11; // chainid
    array[0x47] = 52; // selfbalance
    array[0x48] = 14; // basefee
    array[0x49] = 13; // blobhash
    array[0x4a] = 15; // blobbasefee
    array[0x50] = 10; // pop
    array[0x51] = 18; // mload
    array[0x52] = 29; // mstore
    array[0x53] = 10; // mstore8
    array[0x54] = 3; // sload
    array[0x55] = 5; // sstore
    array[0x56] = 4; // jump
    array[0x57] = 5; // jumpi
    array[0x58] = 13; // pc
    array[0x59] = 13; // msize
    array[0x5a] = 11; // gas
    array[0x5b] = 20; // jumpdest
    array[0x5c] = 1; // tload
    array[0x5d] = 5; // tstore
    array[0x5e] = 4; // mcopy
    array[0x5f] = 13; // push0
    array[0x60] = 9; // push1
    array[0x61] = 8; // push2
    array[0x62] = 9; // push3
    array[0x63] = 10; // push4
    array[0x64] = 9; // push5
    array[0x65] = 12; // push6
    array[0x66] = 10; // push7
    array[0x67] = 15; // push8
    array[0x68] = 13; // push9
    array[0x69] = 12; // push10
    array[0x6a] = 12; // push11
    array[0x6b] = 15; // push12
    array[0x6c] = 15; // push13
    array[0x6d] = 17; // push14
    array[0x6e] = 21; // push15
    array[0x6f] = 13; // push16
    array[0x70] = 20; // push17
    array[0x71] = 18; // push18
    array[0x72] = 20; // push19
    array[0x73] = 20; // push20
    array[0x74] = 18; // push21
    array[0x75] = 14; // push22
    array[0x76] = 22; // push23
    array[0x77] = 24; // push24
    array[0x78] = 22; // push25
    array[0x79] = 24; // push26
    array[0x7a] = 16; // push27
    array[0x7b] = 17; // push28
    array[0x7c] = 28; // push29
    array[0x7d] = 29; // push30
    array[0x7e] = 16; // push31
    array[0x7f] = 19; // push32
    array[0x80] = 10; // dup1
    array[0x81] = 8; // dup2
    array[0x82] = 9; // dup3
    array[0x83] = 10; // dup4
    array[0x84] = 11; // dup5
    array[0x85] = 8; // dup6
    array[0x86] = 8; // dup7
    array[0x87] = 10; // dup8
    array[0x88] = 9; // dup9
    array[0x89] = 10; // dup10
    array[0x8a] = 10; // dup11
    array[0x8b] = 9; // dup12
    array[0x8c] = 9; // dup13
    array[0x8d] = 8; // dup14
    array[0x8e] = 8; // dup15
    array[0x8f] = 10; // dup16
    array[0x90] = 31; // swap1
    array[0x91] = 30; // swap2
    array[0x92] = 32; // swap3
    array[0x93] = 31; // swap4
    array[0x94] = 33; // swap5
    array[0x95] = 34; // swap6
    array[0x96] = 31; // swap7
    array[0x97] = 30; // swap8
    array[0x98] = 32; // swap9
    array[0x99] = 31; // swap10
    array[0x9a] = 33; // swap11
    array[0x9b] = 32; // swap12
    array[0x9c] = 31; // swap13
    array[0x9d] = 31; // swap14
    array[0x9e] = 36; // swap15
    array[0x9f] = 31; // swap16
    array[0xa0] = 3; // log0
    array[0xa1] = 3; // log1
    array[0xa2] = 2; // log2
    array[0xa3] = 2; // log3
    array[0xa4] = 2; // log4
    array[0xf0] = 1; // create
    array[0xf1] = 20; // call
    array[0xf2] = 20; // callcode
    array[0xf3] = 0; // return
    array[0xf4] = 17; // delegatecall
    array[0xf5] = 1; // create2
    array[0xfa] = 23; // staticcall
    array[0xfd] = 0; // revert
    array[0xfe] = 0; // invalid
    array[0xff] = 0; // selfdestruct
    array
}
