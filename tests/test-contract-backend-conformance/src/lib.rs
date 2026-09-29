//! Contract for the Wasm backend conformance suite.
//!
//! The same module runs under Cranelift and Pulley, and every result must match
//! byte for byte. The first byte of the state selects what `validate_state`
//! does, so one module covers normal runs, real traps and heavy work:
//!
//! | First byte | `validate_state` |
//! | --- | --- |
//! | `0xF0` | reads far past the end of linear memory: an out-of-bounds trap |
//! | `0xF1` | divides by a zero read from the state: a divide-by-zero trap |
//! | `0xF2` | panics, which compiles to `unreachable`: an unreachable trap |
//! | `0xF3` | spins until the host's execution-time limit interrupts it |
//! | `0xE0` | hashes the state `n` times, `n` read from bytes 1..5 (u32 LE) |
//! | `0xE1` | allocates and touches `n` MiB, `n` read from byte 1 |
//! | other | accepts the state |
//!
//! The other three functions are plain byte work:
//!
//! - `update_state` appends each delta and replaces the state on a full state.
//! - `summarize_state` returns the state length (u32 LE) and its FNV-1a hash
//!   (u64 LE), 12 bytes in all.
//! - `get_state_delta` returns the bytes past the length in the summary.

use freenet_stdlib::prelude::*;

struct Contract;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let mut raw = [0u8; 4];
    for (i, slot) in raw.iter_mut().enumerate() {
        *slot = bytes.get(at + i).copied().unwrap_or(0);
    }
    u32::from_le_bytes(raw)
}

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let bytes = state.as_ref();
        match bytes.first().copied() {
            None => Err(ContractError::InvalidState),
            Some(0xF0) => {
                // 0xFFFF_FFF0 lies far past any linear memory this module can
                // grow to, so the load traps under every backend.
                let far = core::hint::black_box(0xFFFF_FFF0usize) as *const u32;
                // SAFETY: deliberately unsound. The read exists to make the Wasm
                // engine raise an out-of-bounds trap; it never returns.
                let value = unsafe { core::ptr::read_volatile(far) };
                Ok(if value == 0 {
                    ValidateResult::Valid
                } else {
                    ValidateResult::Invalid
                })
            }
            Some(0xF1) => {
                let divisor = u32::from(core::hint::black_box(bytes.get(1).copied().unwrap_or(0)));
                let dividend = core::hint::black_box(7u32);
                // The Wasm `i32.div_u` instruction itself must trap on the zero.
                let quotient = unsafe_div(dividend, divisor);
                Ok(if quotient == 0 {
                    ValidateResult::Invalid
                } else {
                    ValidateResult::Valid
                })
            }
            Some(0xF2) => panic!("backend conformance: deliberate panic"),
            Some(0xF3) => {
                let mut counter = 0u64;
                loop {
                    counter = core::hint::black_box(counter.wrapping_add(1));
                }
            }
            Some(0xE0) => {
                let rounds = read_u32(bytes, 1);
                let mut hash = FNV_OFFSET;
                for _ in 0..rounds {
                    hash = fnv1a(hash, bytes);
                }
                Ok(if core::hint::black_box(hash) == 0 {
                    ValidateResult::Invalid
                } else {
                    ValidateResult::Valid
                })
            }
            Some(0xE1) => {
                let mib = usize::from(bytes.get(1).copied().unwrap_or(0));
                let mut block = vec![0u8; mib * 1024 * 1024];
                for (i, byte) in block.iter_mut().enumerate().step_by(4096) {
                    *byte = (i % 251) as u8;
                }
                Ok(
                    if core::hint::black_box(&block).len() == mib * 1024 * 1024 {
                        ValidateResult::Valid
                    } else {
                        ValidateResult::Invalid
                    },
                )
            }
            Some(_) => Ok(ValidateResult::Valid),
        }
    }

    fn update_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let mut next = state.as_ref().to_vec();
        for update in data {
            match update {
                UpdateData::Delta(delta) => next.extend_from_slice(delta.as_ref()),
                UpdateData::State(full) => next = full.as_ref().to_vec(),
                UpdateData::StateAndDelta { state, delta } => {
                    next = state.as_ref().to_vec();
                    next.extend_from_slice(delta.as_ref());
                }
                _ => return Err(ContractError::InvalidUpdate),
            }
        }
        Ok(UpdateModification::valid(State::from(next)))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let bytes = state.as_ref();
        let len = u32::try_from(bytes.len()).map_err(|_| ContractError::InvalidState)?;
        let mut summary = Vec::with_capacity(12);
        summary.extend_from_slice(&len.to_le_bytes());
        summary.extend_from_slice(&fnv1a(FNV_OFFSET, bytes).to_le_bytes());
        Ok(StateSummary::from(summary))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        if summary.as_ref().len() != 12 {
            return Err(ContractError::Other("summary must be 12 bytes".to_owned()));
        }
        let known = read_u32(summary.as_ref(), 0) as usize;
        let bytes = state.as_ref();
        if known > bytes.len() {
            return Err(ContractError::Other(
                "summary is longer than the state".to_owned(),
            ));
        }
        Ok(StateDelta::from(bytes[known..].to_vec()))
    }
}

#[inline(never)]
fn unsafe_div(dividend: u32, divisor: u32) -> u32 {
    // Rust's `/` checks for zero and panics, which would test the unreachable
    // trap again. Promising the compiler a non-zero divisor removes that check,
    // so the engine executes `i32.div_u` on zero and raises its own trap.
    // SAFETY: deliberately false when the state asks for this trap. The Wasm
    // division traps before any undefined result can be observed.
    unsafe { core::hint::assert_unchecked(divisor != 0) };
    dividend / divisor
}
