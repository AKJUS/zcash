use std::convert::{TryFrom, TryInto};
use std::io::Cursor;
use std::{ptr, slice};

use blake2b_simd::Hash;
use libc::{c_uchar, size_t};
use tracing::error;
use transparent::sighash::TransparentAuthorizingContext;
use zcash_encoding::Vector;
use zcash_primitives::{
    consensus::BranchId,
    legacy::Script,
    transaction::{
        sighash::SignableInput, sighash_v5::v5_signature_hash, txid::TxIdDigester, Authorization,
        Transaction, TransactionData, TxDigests, TxVersion,
    },
};
use zcash_protocol::value::Zatoshis;

/// Calculates identifying and authorizing digests for the given transaction.
///
/// If either `txid_ret` or `auth_digest_ret` is `nullptr`, the corresponding digest will
/// not be calculated.
///
/// Returns `false` if the transaction is invalid.
#[no_mangle]
pub extern "C" fn zcash_transaction_digests(
    tx_bytes: *const c_uchar,
    tx_bytes_len: size_t,
    txid_ret: *mut [u8; 32],
    auth_digest_ret: *mut [u8; 32],
) -> bool {
    let tx_bytes = unsafe { slice::from_raw_parts(tx_bytes, tx_bytes_len) };

    // We use a placeholder branch ID here, since it is not used for anything.
    let tx = match Transaction::read(tx_bytes, BranchId::Canopy) {
        Ok(tx) => tx,
        Err(e) => {
            error!("Failed to parse transaction: {}", e);
            return false;
        }
    };

    if let Some(txid_ret) = unsafe { txid_ret.as_mut() } {
        *txid_ret = *tx.txid().as_ref();
    }
    if let Some(auth_digest_ret) = unsafe { auth_digest_ret.as_mut() } {
        match tx.version() {
            // Pre-NU5 transaction formats don't have authorizing data commitments; when
            // included in the authDataCommitment tree, they use the [0xff; 32] value.
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => *auth_digest_ret = [0xff; 32],
            _ => auth_digest_ret.copy_from_slice(tx.auth_commitment().as_bytes()),
        }
    }

    true
}

#[derive(Clone, Debug)]
pub(crate) struct TransparentAuth {
    all_prev_outputs: Vec<transparent::bundle::TxOut>,
}

impl transparent::bundle::Authorization for TransparentAuth {
    type ScriptSig = Script;
}

impl TransparentAuthorizingContext for TransparentAuth {
    fn input_amounts(&self) -> Vec<Zatoshis> {
        self.all_prev_outputs
            .iter()
            .map(|prevout| prevout.value)
            .collect()
    }

    fn input_scriptpubkeys(&self) -> Vec<Script> {
        self.all_prev_outputs
            .iter()
            .map(|prevout| prevout.script_pubkey.clone())
            .collect()
    }
}

pub(crate) struct MapTransparent {
    auth: TransparentAuth,
}

impl MapTransparent {
    pub(crate) fn parse(all_prev_outputs: &[u8], tx: &Transaction) -> Result<Self, String> {
        let mut cursor = Cursor::new(all_prev_outputs);
        match Vector::read(&mut cursor, transparent::bundle::TxOut::read) {
            Err(e) => Err(format!("Invalid all_prev_outputs field: {}", e)),
            Ok(_) if (cursor.position() as usize) != all_prev_outputs.len() => {
                Err("all_prev_outputs had trailing data".into())
            }
            Ok(all_prev_outputs)
                if tx.transparent_bundle().map_or(false, |t| {
                    // Coinbase txs have one fake input.
                    t.is_coinbase() && !all_prev_outputs.is_empty()
                }) =>
            {
                Err(format!(
                    "all_prev_outputs should be empty for a coinbase tx but has length {}",
                    all_prev_outputs.len()
                ))
            }
            Ok(all_prev_outputs)
                if tx
                    .transparent_bundle()
                    .map(|t| {
                        // For non-coinbase txs, every input is real.
                        !t.is_coinbase() && t.vin.len() != all_prev_outputs.len()
                    })
                    // If we have no transparent part, we should have no prev outputs.
                    .unwrap_or_else(|| !all_prev_outputs.is_empty()) =>
            {
                Err(format!(
                    "all_prev_outputs is incorrect length {} (should be {})",
                    all_prev_outputs.len(),
                    tx.transparent_bundle().map(|t| t.vin.len()).unwrap_or(0),
                ))
            }
            Ok(all_prev_outputs) => Ok(MapTransparent {
                auth: TransparentAuth { all_prev_outputs },
            }),
        }
    }
}

impl transparent::bundle::MapAuth<transparent::bundle::Authorized, TransparentAuth>
    for MapTransparent
{
    fn map_script_sig(
        &self,
        s: <transparent::bundle::Authorized as transparent::bundle::Authorization>::ScriptSig,
    ) -> <TransparentAuth as transparent::bundle::Authorization>::ScriptSig {
        s
    }

    fn map_authorization(&self, _: transparent::bundle::Authorized) -> TransparentAuth {
        // TODO: This map should consume self, so we can move self.auth
        self.auth.clone()
    }
}

pub(crate) struct PrecomputedAuth;

impl Authorization for PrecomputedAuth {
    type TransparentAuth = TransparentAuth;
    type SaplingAuth = sapling::bundle::Authorized;
    type OrchardAuth = orchard::bundle::Authorized;
}

pub struct PrecomputedTxParts {
    pub(crate) tx: TransactionData<PrecomputedAuth>,
    pub(crate) txid_parts: TxDigests<Hash>,
}

/// Precomputes the `TxDigest` struct for the given transaction.
///
/// Returns `nullptr` if the transaction is invalid, or a v1-v4 transaction format.
#[no_mangle]
pub extern "C" fn zcash_transaction_precomputed_init(
    tx_bytes: *const c_uchar,
    tx_bytes_len: size_t,
    all_prev_outputs: *const c_uchar,
    all_prev_outputs_len: size_t,
) -> *mut PrecomputedTxParts {
    let tx_bytes = unsafe { slice::from_raw_parts(tx_bytes, tx_bytes_len) };

    // We use a placeholder branch ID here, since it is not used for anything.
    //
    // TODO: This is also parsing a transaction that may have partially-filled fields.
    // This doesn't matter for transparent components (the only such field would be the
    // scriptSig fields of transparent inputs, which would serialize as empty Scripts),
    // but is ill-defined for shielded components (we'll be serializing 64 bytes of zeroes
    // for each signature). This is an internal FFI so it's fine for now, but we should
    // refactor the transaction builder (which is the only source of partially-created
    // shielded components) to use a different FFI for obtaining the sighash, that passes
    // across the transaction components and then constructs the TransactionData. This is
    // already being done as part of the Orchard changes to the transaction builder, since
    // the Orchard bundle will already be built on the Rust side, and we can avoid passing
    // it back and forward across the FFI with this change. We should similarly refactor
    // the Sapling code to do the same.
    let tx = match Transaction::read(tx_bytes, BranchId::Canopy) {
        Ok(tx) => tx,
        Err(e) => {
            error!("Failed to parse transaction: {}", e);
            return ptr::null_mut();
        }
    };

    match tx.version() {
        TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => {
            // We don't support these legacy transaction formats in this API.
            ptr::null_mut()
        }
        _ => {
            let all_prev_outputs =
                unsafe { slice::from_raw_parts(all_prev_outputs, all_prev_outputs_len) };

            let f_transparent = match MapTransparent::parse(all_prev_outputs, &tx) {
                Ok(f) => f,
                Err(e) => {
                    error!("{}", e);
                    return ptr::null_mut();
                }
            };

            let tx = tx.into_data().map_authorization(f_transparent, (), ());

            let txid_parts = tx.digest(TxIdDigester);
            Box::into_raw(Box::new(PrecomputedTxParts { tx, txid_parts }))
        }
    }
}

/// Frees a precomputed transaction from `zcash_transaction_precomputed_init`.
#[no_mangle]
pub extern "C" fn zcash_transaction_precomputed_free(precomputed_tx: *mut PrecomputedTxParts) {
    if !precomputed_tx.is_null() {
        drop(unsafe { Box::from_raw(precomputed_tx) });
    }
}

/// This MUST match `NOT_AN_INPUT` in `src/script/interpreter.h`.
const NOT_AN_INPUT: usize = 0xffffffff;

/// Calculates a ZIP 244 signature digest for the given transaction.
///
/// `index` must be an index into the transaction's `vin`, or `NOT_AN_INPUT` for
/// calculating the signature digest for shielded signatures.
///
/// `sighash_ret` must point to a 32-byte array.
///
/// Returns `false` if any of the parameters are invalid; in this case, `sighash_ret`
/// will be unaltered.
#[no_mangle]
pub extern "C" fn zcash_transaction_zip244_signature_digest(
    precomputed_tx: *const PrecomputedTxParts,
    hash_type: u32,
    index: size_t,
    sighash_ret: *mut [u8; 32],
) -> bool {
    let precomputed_tx = if let Some(res) = unsafe { precomputed_tx.as_ref() } {
        res
    } else {
        error!("Invalid precomputed transaction");
        return false;
    };
    if matches!(
        precomputed_tx.tx.version(),
        TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4,
    ) {
        error!("Cannot calculate ZIP 244 digest for pre-v5 transaction");
        return false;
    }

    let signable_input = if index == NOT_AN_INPUT {
        SignableInput::Shielded
    } else {
        // This conversion to `u8` is always fine:
        // - We only call this FFI method once we already know we are using ZIP 244.
        // - Even if we weren't, `hash_type` is one byte tacked onto the end of a
        //   signature, so it always fits into a `u8` (and TBH I don't know why we
        //   ever set it to `u32`).
        let hash_type = u8::try_from(hash_type).unwrap();

        let hash_type = match transparent::sighash::SighashType::parse(hash_type) {
            Some(hash_type) => hash_type,
            None => {
                error!("hash_type violates the ZIP 244 rules");
                return false;
            }
        };

        let prevout = match precomputed_tx.tx.transparent_bundle() {
            Some(bundle) => match bundle.authorization.all_prev_outputs.get(index) {
                Some(prevout) => prevout,
                None => {
                    error!("nIn out of range");
                    return false;
                }
            },
            None => {
                error!("Tried to create a transparent sighash for a tx without transparent data");
                return false;
            }
        };

        SignableInput::Transparent(transparent::sighash::SignableInput::from_parts(
            hash_type,
            index,
            // `script_code` is unused by `v5_signature_hash`, so instead of passing the
            // real `script_code` across the FFI (and paying the serialization and parsing
            // cost for no benefit), we set it to the prevout's `script_pubkey`. This
            // happens to be correct anyway for every output script kind except P2SH.
            &prevout.script_pubkey,
            &prevout.script_pubkey,
            prevout.value,
        ))
    };

    let sighash = v5_signature_hash(
        &precomputed_tx.tx,
        &signable_input,
        &precomputed_tx.txid_parts,
    );

    // `v5_signature_hash` output is always 32 bytes.
    *unsafe { &mut *sighash_ret } = sighash.as_ref().try_into().unwrap();
    true
}
