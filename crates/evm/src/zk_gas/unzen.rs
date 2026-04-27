//! Fixed Unzen zk gas schedule constants copied from the approved protocol spec.
//!
//! Source:
//! <https://github.com/taikoxyz/taiko-mono/blob/main/packages/protocol/docs/zk_gas_spec.md>

use super::schedule::{SpawnEstimates, ZkGasSchedule};

/// Maximum zk gas permitted within a single Unzen block on Devnet, Hoodi, and Mainnet.
pub const BLOCK_ZK_GAS_LIMIT: u64 = 100_000_000;

/// Maximum zk gas permitted within a single Unzen block on the Taiko Masaya network.
pub const MASAYA_BLOCK_ZK_GAS_LIMIT: u64 = 1_000_000_000;

/// Fail-safe multiplier used for any opcode or precompile missing from the Unzen table.
const FAILSAFE_MULTIPLIER: u16 = u16::MAX;

/// Builds an Unzen-shaped zk gas schedule with the requested block limit. Opcode multipliers,
/// precompile multipliers, and spawn estimates are identical across all networks; only the
/// block budget differs.
const fn unzen_schedule_with_block_limit(block_limit: u64) -> ZkGasSchedule {
    ZkGasSchedule {
        block_limit,
        opcode_multipliers: unzen_opcode_multipliers(),
        precompile_multipliers: unzen_precompile_multipliers(),
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

/// Default Unzen zk gas schedule used by Devnet, Hoodi, and Mainnet.
pub const UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule =
    unzen_schedule_with_block_limit(BLOCK_ZK_GAS_LIMIT);

/// Unzen zk gas schedule used by the Taiko Masaya network with a 10× higher block budget.
pub const MASAYA_UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule =
    unzen_schedule_with_block_limit(MASAYA_BLOCK_ZK_GAS_LIMIT);

/// Returns the fixed Unzen opcode multiplier table with fail-safe defaults for unlisted opcodes.
const fn unzen_opcode_multipliers() -> [u16; 256] {
    let mut array = [FAILSAFE_MULTIPLIER; 256];
    array[0x09] = 152; // mulmod
    array[0x04] = 110; // div
    array[0x06] = 95; // mod
    array[0x05] = 93; // sdiv
    array[0x47] = 85; // selfbalance
    array[0x20] = 85; // keccak256
    array[0x08] = 71; // addmod
    array[0x14] = 35; // eq
    array[0x0a] = 33; // exp
    array[0x07] = 29; // smod
    array[0x1d] = 29; // sar
    array[0x44] = 28; // prevrandao
    array[0xf1] = 25; // call
    array[0xf2] = 24; // callcode
    array[0xfa] = 24; // staticcall
    array[0x52] = 22; // mstore
    array[0x30] = 22; // address
    array[0x32] = 21; // origin
    array[0x33] = 21; // caller
    array[0x02] = 21; // mul
    array[0xf4] = 21; // delegatecall
    array[0x41] = 21; // coinbase
    array[0x0b] = 21; // signextend
    array[0x1b] = 20; // shl
    array[0x35] = 20; // calldataload
    array[0x51] = 20; // mload
    array[0x93] = 19; // swap4
    array[0x9c] = 19; // swap13
    array[0x1c] = 19; // shr
    array[0x9b] = 19; // swap12
    array[0x9a] = 18; // swap11
    array[0x92] = 18; // swap3
    array[0x9d] = 18; // swap14
    array[0x98] = 18; // swap9
    array[0x91] = 18; // swap2
    array[0x7e] = 18; // push31
    array[0x9f] = 18; // swap16
    array[0x9e] = 17; // swap15
    array[0x7c] = 17; // push29
    array[0x7b] = 17; // push28
    array[0x96] = 17; // swap7
    array[0x95] = 17; // swap6
    array[0x99] = 17; // swap10
    array[0x7f] = 17; // push32
    array[0x90] = 16; // swap1
    array[0x77] = 16; // push24
    array[0x94] = 16; // swap5
    array[0x75] = 16; // push22
    array[0x97] = 15; // swap8
    array[0x7a] = 15; // push27
    array[0x4a] = 15; // blobbasefee
    array[0x3a] = 14; // gasprice
    array[0x79] = 14; // push26
    array[0x12] = 14; // slt
    array[0x74] = 14; // push21
    array[0x13] = 14; // sgt
    array[0x03] = 13; // sub
    array[0x34] = 13; // callvalue
    array[0x78] = 13; // push25
    array[0x70] = 13; // push17
    array[0x73] = 13; // push20
    array[0x39] = 13; // codecopy
    array[0x55] = 13; // sstore
    array[0x6d] = 12; // push14
    array[0x37] = 12; // calldatacopy
    array[0x7d] = 12; // push30
    array[0x76] = 12; // push23
    array[0x58] = 12; // pc
    array[0x01] = 12; // add
    array[0x72] = 11; // push19
    array[0x5a] = 11; // gas
    array[0x42] = 11; // timestamp
    array[0x48] = 11; // basefee
    array[0x43] = 11; // number
    array[0x71] = 11; // push18
    array[0x36] = 11; // calldatasize
    array[0x6f] = 11; // push16
    array[0x38] = 11; // codesize
    array[0x46] = 11; // chainid
    array[0x10] = 11; // lt
    array[0x45] = 11; // gaslimit
    array[0x59] = 11; // msize
    array[0x3d] = 10; // returndatasize
    array[0x5f] = 10; // push0
    array[0x6e] = 10; // push15
    array[0x11] = 10; // gt
    array[0x69] = 10; // push10
    array[0x49] = 10; // blobhash
    array[0x6b] = 9; // push12
    array[0x68] = 9; // push9
    array[0x17] = 9; // or
    array[0x53] = 9; // mstore8
    array[0x1a] = 9; // byte
    array[0x18] = 9; // xor
    array[0x5b] = 9; // jumpdest
    array[0x3e] = 9; // returndatacopy
    array[0x6a] = 8; // push11
    array[0x16] = 8; // and
    array[0x8b] = 8; // dup12
    array[0x8e] = 8; // dup15
    array[0x65] = 8; // push6
    array[0x63] = 8; // push4
    array[0x15] = 8; // iszero
    array[0x3f] = 8; // extcodehash
    array[0x6c] = 7; // push13
    array[0x66] = 7; // push7
    array[0x40] = 7; // blockhash
    array[0x88] = 7; // dup9
    array[0x67] = 7; // push8
    array[0x8d] = 7; // dup14
    array[0xa1] = 7; // log1
    array[0xa0] = 6; // log0
    array[0x19] = 6; // not
    array[0x8f] = 6; // dup16
    array[0x84] = 6; // dup5
    array[0x62] = 6; // push3
    array[0x85] = 6; // dup6
    array[0x87] = 6; // dup8
    array[0x3b] = 6; // extcodesize
    array[0x31] = 6; // balance
    array[0x80] = 6; // dup1
    array[0x82] = 6; // dup3
    array[0x5d] = 6; // tstore
    array[0x8c] = 6; // dup13
    array[0x8a] = 6; // dup11
    array[0x3c] = 6; // extcodecopy
    array[0x83] = 6; // dup4
    array[0x64] = 6; // push5
    array[0x50] = 5; // pop
    array[0x54] = 5; // sload
    array[0x5e] = 5; // mcopy
    array[0x60] = 5; // push1
    array[0x89] = 5; // dup10
    array[0x61] = 5; // push2
    array[0x86] = 5; // dup7
    array[0xa3] = 5; // log3
    array[0x81] = 5; // dup2
    array[0xa4] = 5; // log4
    array[0x57] = 5; // jumpi
    array[0xa2] = 4; // log2
    array[0x56] = 3; // jump
    array[0x5c] = 1; // tload
    array[0xf0] = 1; // create
    array[0xf5] = 1; // create2
    array[0x00] = 0; // stop
    array[0xf3] = 0; // return
    array[0xfd] = 0; // revert
    array[0xff] = 0; // selfdestruct
    array[0xfe] = 0; // invalid
    array
}

/// Returns the fixed Unzen precompile multiplier table with fail-safe defaults for unlisted
/// entries.
const fn unzen_precompile_multipliers() -> [u16; 256] {
    let mut array = [FAILSAFE_MULTIPLIER; 256];
    array[0x05] = 1363; // modexp
    array[0x0a] = 398; // point_evaluation
    array[0x09] = 243; // blake2f
    array[0x12] = 159; // bls12_map_fp_to_g1
    array[0x11] = 134; // bls12_pairing
    array[0x0b] = 112; // bls12_g1add
    array[0x13] = 112; // bls12_map_fp2_to_g2
    array[0x0e] = 111; // bls12_g2add
    array[0x07] = 87; // bn128_mul
    array[0x08] = 82; // bn128_pairing
    array[0x01] = 81; // ecrecover
    array[0x0c] = 52; // bls12_g1msm
    array[0x0f] = 39; // bls12_g2msm
    array[0x06] = 38; // bn128_add
    array[0x02] = 10; // sha256
    array[0x03] = 3; // ripemd160
    array[0x04] = 2; // identity
    array
}
