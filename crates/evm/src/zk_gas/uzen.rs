//! Fixed Uzen zk gas schedule constants copied from the approved protocol spec.

use super::schedule::{SpawnEstimates, ZkGasSchedule};

/// Maximum zk gas permitted within a single Uzen block.
pub const BLOCK_ZK_GAS_LIMIT: u64 = 100_000_000;

/// Fail-safe multiplier used for any opcode or precompile missing from the Uzen table.
const FAILSAFE_MULTIPLIER: u16 = u16::MAX;

/// Fixed Uzen zk gas schedule used for consensus execution.
pub const UZEN_ZK_GAS_SCHEDULE: ZkGasSchedule = ZkGasSchedule {
    block_limit: BLOCK_ZK_GAS_LIMIT,
    opcode_multipliers: uzen_opcode_multipliers(),
    precompile_multipliers: uzen_precompile_multipliers(),
    spawn_estimates: SpawnEstimates {
        call: 12_500,
        callcode: 12_500,
        delegatecall: 3_500,
        staticcall: 3_500,
        create: 37_000,
        create2: 44_500,
    },
};

/// Returns the fixed Uzen opcode multiplier table with fail-safe defaults for unlisted opcodes.
const fn uzen_opcode_multipliers() -> [u16; 256] {
    let mut array = [FAILSAFE_MULTIPLIER; 256];
    array[0x09] = 152;
    array[0x04] = 110;
    array[0x06] = 95;
    array[0x05] = 93;
    array[0x47] = 85;
    array[0x20] = 85;
    array[0x08] = 71;
    array[0x14] = 35;
    array[0x0a] = 33;
    array[0x07] = 29;
    array[0x1d] = 29;
    array[0x44] = 28;
    array[0xf1] = 25;
    array[0xf2] = 24;
    array[0xfa] = 24;
    array[0x52] = 22;
    array[0x30] = 22;
    array[0x32] = 21;
    array[0x33] = 21;
    array[0x02] = 21;
    array[0xf4] = 21;
    array[0x41] = 21;
    array[0x0b] = 21;
    array[0x1b] = 20;
    array[0x35] = 20;
    array[0x51] = 20;
    array[0x93] = 19;
    array[0x9c] = 19;
    array[0x1c] = 19;
    array[0x9b] = 19;
    array[0x9a] = 18;
    array[0x92] = 18;
    array[0x9d] = 18;
    array[0x98] = 18;
    array[0x91] = 18;
    array[0x7e] = 18;
    array[0x9f] = 18;
    array[0x9e] = 17;
    array[0x7c] = 17;
    array[0x7b] = 17;
    array[0x96] = 17;
    array[0x95] = 17;
    array[0x99] = 17;
    array[0x7f] = 17;
    array[0x90] = 16;
    array[0x77] = 16;
    array[0x94] = 16;
    array[0x75] = 16;
    array[0x97] = 15;
    array[0x7a] = 15;
    array[0x4a] = 15;
    array[0x3a] = 14;
    array[0x79] = 14;
    array[0x12] = 14;
    array[0x74] = 14;
    array[0x13] = 14;
    array[0x03] = 13;
    array[0x34] = 13;
    array[0x78] = 13;
    array[0x70] = 13;
    array[0x73] = 13;
    array[0x39] = 13;
    array[0x55] = 13;
    array[0x6d] = 12;
    array[0x37] = 12;
    array[0x7d] = 12;
    array[0x76] = 12;
    array[0x58] = 12;
    array[0x01] = 12;
    array[0x72] = 11;
    array[0x5a] = 11;
    array[0x42] = 11;
    array[0x48] = 11;
    array[0x43] = 11;
    array[0x71] = 11;
    array[0x36] = 11;
    array[0x6f] = 11;
    array[0x38] = 11;
    array[0x46] = 11;
    array[0x10] = 11;
    array[0x45] = 11;
    array[0x59] = 11;
    array[0x3d] = 10;
    array[0x5f] = 10;
    array[0x6e] = 10;
    array[0x11] = 10;
    array[0x69] = 10;
    array[0x49] = 10;
    array[0x6b] = 9;
    array[0x68] = 9;
    array[0x17] = 9;
    array[0x53] = 9;
    array[0x1a] = 9;
    array[0x18] = 9;
    array[0x5b] = 9;
    array[0x3e] = 9;
    array[0x6a] = 8;
    array[0x16] = 8;
    array[0x8b] = 8;
    array[0x8e] = 8;
    array[0x65] = 8;
    array[0x63] = 8;
    array[0x15] = 8;
    array[0x3f] = 8;
    array[0x6c] = 7;
    array[0x66] = 7;
    array[0x40] = 7;
    array[0x88] = 7;
    array[0x67] = 7;
    array[0x8d] = 7;
    array[0xa1] = 7;
    array[0xa0] = 6;
    array[0x19] = 6;
    array[0x8f] = 6;
    array[0x84] = 6;
    array[0x62] = 6;
    array[0x85] = 6;
    array[0x87] = 6;
    array[0x3b] = 6;
    array[0x31] = 6;
    array[0x80] = 6;
    array[0x82] = 6;
    array[0x5d] = 6;
    array[0x8c] = 6;
    array[0x8a] = 6;
    array[0x3c] = 6;
    array[0x83] = 6;
    array[0x64] = 6;
    array[0x50] = 5;
    array[0x54] = 5;
    array[0x5e] = 5;
    array[0x60] = 5;
    array[0x89] = 5;
    array[0x61] = 5;
    array[0x86] = 5;
    array[0xa3] = 5;
    array[0x81] = 5;
    array[0xa4] = 5;
    array[0x57] = 5;
    array[0xa2] = 4;
    array[0x56] = 3;
    array[0x5c] = 1;
    array[0xf0] = 1;
    array[0xf5] = 1;
    array[0x00] = 0;
    array[0xf3] = 0;
    array[0xfd] = 0;
    array[0xff] = 0;
    array[0xfe] = 0;
    array
}

/// Returns the fixed Uzen precompile multiplier table with fail-safe defaults for unlisted entries.
const fn uzen_precompile_multipliers() -> [u16; 256] {
    let mut array = [FAILSAFE_MULTIPLIER; 256];
    array[0x05] = 1363;
    array[0x0a] = 398;
    array[0x09] = 243;
    array[0x12] = 159;
    array[0x11] = 134;
    array[0x0b] = 112;
    array[0x13] = 112;
    array[0x0e] = 111;
    array[0x07] = 87;
    array[0x08] = 82;
    array[0x01] = 81;
    array[0x0c] = 52;
    array[0x0f] = 39;
    array[0x06] = 38;
    array[0x02] = 10;
    array[0x03] = 3;
    array[0x04] = 2;
    array
}
