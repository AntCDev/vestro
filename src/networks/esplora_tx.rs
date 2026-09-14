use k256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use sha2::{Digest, Sha256};

pub const SIGHASH_ALL: u32 = 1;
/// Opt-in RBF. Costs nothing now, lets you fee-bump a stuck sweep later.
pub const SEQUENCE_RBF: u32 = 0xffff_fffd;

/// Bitcoin Core's dust floor for a P2WPKH output at the default 3 sat/vB
/// relay fee. An output below this is non-standard and the tx won't relay.
pub const DUST_P2WPKH: u64 = 294;
/// Conservative floor for destinations we didn't derive (P2PKH is 546).
pub const DUST_LEGACY: u64 = 546;

pub fn dsha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(b)).into()
}

pub fn varint(n: u64, out: &mut Vec<u8>) {
    match n {
        0..=0xfc => out.push(n as u8),
        0xfd..=0xffff => { out.push(0xfd); out.extend_from_slice(&(n as u16).to_le_bytes()); }
        0x1_0000..=0xffff_ffff => { out.push(0xfe); out.extend_from_slice(&(n as u32).to_le_bytes()); }
        _ => { out.push(0xff); out.extend_from_slice(&n.to_le_bytes()); }
    }
}

pub fn varint_len(n: usize) -> usize {
    match n { 0..=0xfc => 1, 0xfd..=0xffff => 3, 0x1_0000..=0xffff_ffff => 5, _ => 9 }
}

fn push_script(s: &[u8], out: &mut Vec<u8>) {
    varint(s.len() as u64, out);
    out.extend_from_slice(s);
}

/// Esplora hands out display-order txids (reversed). Internal serialization
/// wants the raw little-endian bytes. Every conversion goes through here.
pub fn txid_from_display(hex_str: &str) -> Result<[u8; 32], String> {
    let mut b = hex::decode(hex_str).map_err(|e| format!("txid {hex_str}: {e}"))?;
    if b.len() != 32 { return Err(format!("txid {hex_str}: not 32 bytes")); }
    b.reverse();
    Ok(b.try_into().unwrap())
}

pub fn txid_to_display(le: &[u8; 32]) -> String {
    let mut b = *le;
    b.reverse();
    hex::encode(b)
}

#[derive(Clone, Debug)]
pub struct TxIn {
    pub txid: [u8; 32], // internal LE order
    pub vout: u32,
    pub value: u64,
}

#[derive(Clone, Debug)]
pub struct TxOut {
    pub value: u64,
    pub script: Vec<u8>,
}

// ── scriptPubKey ────────────────────────────────────────────────────────────

/// Accepts anything `validate_address` accepts: bech32/bech32m segwit, and
/// legacy P2PKH/P2SH for merchant-supplied destinations.
pub fn script_pubkey(address: &str, base58: impl Fn(&str) -> Option<Vec<u8>>) -> Result<Vec<u8>, String> {
    if let Ok((_hrp, version, program)) = bech32::segwit::decode(address) {
        let v = version.to_u8();
        let mut s = Vec::with_capacity(2 + program.len());
        s.push(if v == 0 { 0x00 } else { 0x50 + v });
        s.push(program.len() as u8);
        s.extend_from_slice(&program);
        return Ok(s);
    }
    let payload = base58(address).ok_or_else(|| format!("unrecognised address: {address}"))?;
    if payload.len() != 21 { return Err(format!("address {address}: bad payload length")); }
    let h = &payload[1..];
    // 0x00/0x6f = P2PKH, 0x05/0xc4 = P2SH. Version byte was already checked by
    // validate_address against the configured network.
    Ok(match payload[0] {
        0x00 | 0x6f => {
            let mut s = vec![0x76, 0xa9, 0x14];
            s.extend_from_slice(h);
            s.extend_from_slice(&[0x88, 0xac]);
            s
        }
        0x05 | 0xc4 => {
            let mut s = vec![0xa9, 0x14];
            s.extend_from_slice(h);
            s.push(0x87);
            s
        }
        v => return Err(format!("address {address}: unknown version byte {v:#04x}")),
    })
}

pub fn dust_floor(script: &[u8]) -> u64 {
    // v0 20-byte program == P2WPKH.
    if script.len() == 22 && script[0] == 0x00 { DUST_P2WPKH } else { DUST_LEGACY }
}

// ── size / fee ──────────────────────────────────────────────────────────────

/// vbytes for an all-P2WPKH-input transaction. Witness per input is
/// 1 (item count) + 1 + 72 (max DER sig + hashtype) + 1 + 33 (pubkey) = 108.
pub fn vsize(n_in: usize, outs: &[TxOut]) -> u64 {
    let base = 4
        + varint_len(n_in) + n_in * 41
        + varint_len(outs.len())
        + outs.iter().map(|o| 8 + varint_len(o.script.len()) + o.script.len()).sum::<usize>()
        + 4;
    let witness = 2 + n_in * 108;
    ((base * 4 + witness + 3) / 4) as u64
}

// ── BIP143 ──────────────────────────────────────────────────────────────────

fn scriptcode_p2wpkh(h160: &[u8; 20]) -> Vec<u8> {
    let mut s = vec![0x76, 0xa9, 0x14];
    s.extend_from_slice(h160);
    s.extend_from_slice(&[0x88, 0xac]);
    s
}

/// Every input here is P2WPKH controlled by the same key, so hashPrevouts /
/// hashSequence / hashOutputs are computed once and reused across inputs.
struct Bip143Cache {
    prevouts: [u8; 32],
    sequence: [u8; 32],
    outputs: [u8; 32],
}

fn bip143_cache(ins: &[TxIn], outs: &[TxOut]) -> Bip143Cache {
    let mut p = Vec::with_capacity(ins.len() * 36);
    let mut s = Vec::with_capacity(ins.len() * 4);
    for i in ins {
        p.extend_from_slice(&i.txid);
        p.extend_from_slice(&i.vout.to_le_bytes());
        s.extend_from_slice(&SEQUENCE_RBF.to_le_bytes());
    }
    let mut o = Vec::new();
    for out in outs {
        o.extend_from_slice(&out.value.to_le_bytes());
        push_script(&out.script, &mut o);
    }
    Bip143Cache { prevouts: dsha256(&p), sequence: dsha256(&s), outputs: dsha256(&o) }
}

fn sighash(c: &Bip143Cache, input: &TxIn, h160: &[u8; 20], locktime: u32) -> [u8; 32] {
    let mut pre = Vec::with_capacity(200);
    pre.extend_from_slice(&2i32.to_le_bytes()); // nVersion
    pre.extend_from_slice(&c.prevouts);
    pre.extend_from_slice(&c.sequence);
    pre.extend_from_slice(&input.txid);
    pre.extend_from_slice(&input.vout.to_le_bytes());
    push_script(&scriptcode_p2wpkh(h160), &mut pre);
    pre.extend_from_slice(&input.value.to_le_bytes());
    pre.extend_from_slice(&SEQUENCE_RBF.to_le_bytes());
    pre.extend_from_slice(&c.outputs);
    pre.extend_from_slice(&locktime.to_le_bytes());
    pre.extend_from_slice(&SIGHASH_ALL.to_le_bytes());
    dsha256(&pre)
}

// ── DER ─────────────────────────────────────────────────────────────────────
//
// Hand-rolled rather than `Signature::to_der()` so this doesn't depend on
// k256's `der` feature being on. 20 lines, and the format is frozen.

fn der_int(b: &[u8], out: &mut Vec<u8>) {
    let start = b.iter().position(|&x| x != 0).unwrap_or(b.len() - 1);
    let b = &b[start..];
    out.push(0x02);
    if b[0] & 0x80 != 0 {
        out.push(b.len() as u8 + 1);
        out.push(0x00);
    } else {
        out.push(b.len() as u8);
    }
    out.extend_from_slice(b);
}

fn der_encode(sig: &Signature) -> Vec<u8> {
    let r = sig.r().to_bytes();
    let s = sig.s().to_bytes();
    let mut body = Vec::with_capacity(72);
    der_int(&r, &mut body);
    der_int(&s, &mut body);
    let mut out = vec![0x30, body.len() as u8];
    out.extend_from_slice(&body);
    out
}

// ── build ───────────────────────────────────────────────────────────────────

/// Signs every input with `key` (all inputs are P2WPKH for that key) and
/// serialises the segwit transaction. Returns (raw_bytes, display_txid).
pub fn sign_p2wpkh(
    key: &SigningKey,
    ins: &[TxIn],
    outs: &[TxOut],
    locktime: u32,
) -> Result<(Vec<u8>, String), String> {
    if ins.is_empty() { return Err("no inputs".into()); }

    let point = key.verifying_key().to_encoded_point(true);
    let pubkey = point.as_bytes().to_vec();
    let h160: [u8; 20] = ripemd::Ripemd160::digest(Sha256::digest(&pubkey)).into();

    let cache = bip143_cache(ins, outs);
    let mut witnesses: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(ins.len());
    for i in ins {
        let h = sighash(&cache, i, &h160, locktime);
        // k256 normalises to low-S on signing; BIP62 requires it.
        let sig: Signature = key.sign_prehash(&h).map_err(|e| format!("secp256k1 sign: {e}"))?;
        let mut der = der_encode(&sig);
        der.push(SIGHASH_ALL as u8);
        witnesses.push((der, pubkey.clone()));
    }

    // Legacy (no-witness) serialisation → txid.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&2i32.to_le_bytes());
    varint(ins.len() as u64, &mut legacy);
    for i in ins {
        legacy.extend_from_slice(&i.txid);
        legacy.extend_from_slice(&i.vout.to_le_bytes());
        legacy.push(0x00); // empty scriptSig
        legacy.extend_from_slice(&SEQUENCE_RBF.to_le_bytes());
    }
    varint(outs.len() as u64, &mut legacy);
    for o in outs {
        legacy.extend_from_slice(&o.value.to_le_bytes());
        push_script(&o.script, &mut legacy);
    }
    legacy.extend_from_slice(&locktime.to_le_bytes());
    let txid = txid_to_display(&dsha256(&legacy));

    // Wire (witness) serialisation → what we broadcast.
    let mut raw = Vec::with_capacity(legacy.len() + ins.len() * 110);
    raw.extend_from_slice(&2i32.to_le_bytes());
    raw.extend_from_slice(&[0x00, 0x01]); // marker, flag
    varint(ins.len() as u64, &mut raw);
    for i in ins {
        raw.extend_from_slice(&i.txid);
        raw.extend_from_slice(&i.vout.to_le_bytes());
        raw.push(0x00);
        raw.extend_from_slice(&SEQUENCE_RBF.to_le_bytes());
    }
    varint(outs.len() as u64, &mut raw);
    for o in outs {
        raw.extend_from_slice(&o.value.to_le_bytes());
        push_script(&o.script, &mut raw);
    }
    for (sig, pk) in &witnesses {
        varint(2, &mut raw);
        push_script(sig, &mut raw);
        push_script(pk, &mut raw);
    }
    raw.extend_from_slice(&locktime.to_le_bytes());

    Ok((raw, txid))
}

fn out_section_len(outs: &[TxOut]) -> usize {
    varint_len(outs.len()) + outs.iter().map(|o| 8 + varint_len(o.script.len()) + o.script.len()).sum::<usize>()
}

/// Outpoints of a signed transaction, read back off the wire bytes. Used by
/// `transfer_status` to decide Expired vs Unknown without widening
/// `SignedTransfer`.
pub fn parse_outpoints(raw: &[u8]) -> Result<Vec<(String, u32)>, String> {
    let mut p = 4usize;
    if raw.len() < 6 { return Err("raw tx too short".into()); }
    if raw[p] == 0x00 && raw[p + 1] == 0x01 { p += 2; }
    let (n, used) = read_varint(raw, p)?;
    p += used;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        if p + 36 > raw.len() { return Err("truncated input".into()); }
        let mut txid: [u8; 32] = raw[p..p + 32].try_into().unwrap();
        let vout = u32::from_le_bytes(raw[p + 32..p + 36].try_into().unwrap());
        out.push((txid_to_display(&mut txid), vout));
        p += 36;
        let (slen, used) = read_varint(raw, p)?;
        p += used + slen as usize + 4;
    }
    Ok(out)
}

fn read_varint(b: &[u8], p: usize) -> Result<(u64, usize), String> {
    match b.get(p) {
        None => Err("truncated varint".into()),
        Some(&n) if n < 0xfd => Ok((n as u64, 1)),
        Some(&0xfd) => Ok((u16::from_le_bytes(b[p + 1..p + 3].try_into().map_err(|_| "varint")?) as u64, 3)),
        Some(&0xfe) => Ok((u32::from_le_bytes(b[p + 1..p + 5].try_into().map_err(|_| "varint")?) as u64, 5)),
        Some(_) => Ok((u64::from_le_bytes(b[p + 1..p + 9].try_into().map_err(|_| "varint")?), 9)),
    }
}