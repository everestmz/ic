//! Methods for forward secure encryption
use zeroize::Zeroize;

use std::collections::LinkedList;
use std::vec::Vec;

// NOTE: the paper uses multiplicative notation for operations on G1, G2, GT,
// while miracl's API uses additive naming convention, hence
//    u*v  corresponds to u.add(v)
// and
//    g^x  corresponds to g.mul(x)

use crate::ni_dkg::fs_ni_dkg::encryption_key_pop::{
    prove_pop, verify_pop, EncryptionKeyInstance, EncryptionKeyPop,
};
use crate::ni_dkg::fs_ni_dkg::nizk_chunking::CHALLENGE_BITS;
use crate::ni_dkg::fs_ni_dkg::nizk_chunking::NUM_ZK_REPETITIONS;
use crate::ni_dkg::fs_ni_dkg::random_oracles::{random_oracle, HashedMap};
use crate::ni_dkg::fs_ni_dkg::utils::*;
use ic_crypto_internal_bls12_381_type::{G1Affine, G2Affine, G2Prepared, Gt, Scalar};
use ic_crypto_internal_types::sign::threshold_sig::ni_dkg::Epoch;
use lazy_static::lazy_static;
use miracl_core::bls12381::ecp::ECP;
use miracl_core::bls12381::ecp2::ECP2;
use miracl_core::bls12381::fp12::FP12;
use miracl_core::bls12381::rom;
use miracl_core::bls12381::{big, big::BIG};
use miracl_core::rand::RAND;

lazy_static! {
    static ref PRECOMP_SYS_H: G2Prepared = precomp_sys_h();
}

fn precomp_sys_h() -> G2Prepared {
    let sys = mk_sys_params();
    G2Prepared::from(G2Affine::from_miracl(&sys.h))
}

const FP12_SIZE: usize = 12 * big::MODBYTES;

/// The ciphertext is an element of Fr which is 256-bits
pub(crate) const MESSAGE_BYTES: usize = 32;

/// The size in bytes of a chunk
pub const CHUNK_BYTES: usize = 2;

/// The maximum value of a chunk
pub const CHUNK_SIZE: isize = 1 << (CHUNK_BYTES << 3); // Number of distinct chunks

/// The minimum range of a chunk
pub const CHUNK_MIN: isize = 0;

/// The maximum range of a chunk
pub const CHUNK_MAX: isize = CHUNK_MIN + CHUNK_SIZE - 1;

/// NUM_CHUNKS is simply the number of chunks needed to hold a message (element
/// of Fr)
pub const NUM_CHUNKS: usize = (MESSAGE_BYTES + CHUNK_BYTES - 1) / CHUNK_BYTES;

const DOMAIN_CIPHERTEXT_NODE: &str = "ic-fs-encryption/binary-tree-node";

/// Type for a single bit
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Bit {
    Zero = 0,
    One = 1,
}

impl From<u8> for Bit {
    fn from(i: u8) -> Self {
        if i == 0 {
            Bit::Zero
        } else {
            Bit::One
        }
    }
}

impl From<&Bit> for u8 {
    fn from(b: &Bit) -> u8 {
        match &b {
            Bit::Zero => 0,
            Bit::One => 1,
        }
    }
}

impl From<&Bit> for i32 {
    fn from(b: &Bit) -> i32 {
        match &b {
            Bit::Zero => 0,
            Bit::One => 1,
        }
    }
}

/// Generates tau (a vector of bits) from an epoch.
pub fn tau_from_epoch(sys: &SysParam, epoch: Epoch) -> Vec<Bit> {
    (0..sys.lambda_t)
        .rev()
        .map(|index| {
            if (epoch.get() >> index) & 1 == 0 {
                Bit::Zero
            } else {
                Bit::One
            }
        })
        .collect()
}

/// Converts an epoch prefix to an epoch by filling in remaining bits with
/// zeros.
pub fn epoch_from_tau_vec(tau: &[Bit]) -> Epoch {
    let num_bits = ::std::mem::size_of::<Epoch>() * 8;
    Epoch::from(
        (0..num_bits)
            .rev()
            .zip(tau)
            .fold(0u32, |epoch, (shift, tau)| {
                epoch
                    | ((match *tau {
                        Bit::One => 1,
                        Bit::Zero => 0,
                    }) << shift)
            }),
    )
}

/// A node of a Binary Tree Encryption scheme.
///
/// Notation from section 7.2.
pub struct BTENode {
    // Bit-vector, indicating a path in a binary tree.
    pub tau: Vec<Bit>,

    pub a: ECP,
    pub b: ECP2,

    // We split the d's into two groups.
    // The vector `d_h` always contains the last lambda_H points
    // of d_l,...,d_lambda.
    // The list `d_t` contains the other elements. There are at most lambda_T of them.
    // The longer this list, the higher up we are in the binary tree,
    // and the more leaf node keys we are able to derive.
    pub d_t: LinkedList<ECP2>,
    pub d_h: Vec<ECP2>,

    pub e: ECP2,
}

impl zeroize::Zeroize for BTENode {
    fn zeroize(&mut self) {
        self.tau.iter_mut().for_each(|t| *t = Bit::Zero);
        // Overwrite all group elements with generators.
        let g1 = ECP::generator();
        let g2 = ECP2::generator();
        self.a.copy(&g1);
        self.b.copy(&g2);
        self.d_h.iter_mut().for_each(|x| x.copy(&g2));
        self.d_t.iter_mut().for_each(|x| x.copy(&g2));
        self.e.copy(&g2);
    }
}

/// A forward-secure secret key is a list of BTE nodes.
///
/// We can derive the keys of any descendant of any node in the list.
/// We obtain forward security by maintaining the list so that we can
/// derive current and future private keys, but none of the past keys.
pub struct SecretKey {
    pub bte_nodes: LinkedList<BTENode>,
}

/// A public key and its associated proof of possession
#[derive(Clone)]
pub struct PublicKeyWithPop {
    pub key_value: ECP,
    pub proof_data: EncryptionKeyPop,
}

impl PublicKeyWithPop {
    pub fn verify(&self, associated_data: &[u8]) -> bool {
        let instance = EncryptionKeyInstance {
            g1_gen: G1Affine::generator(),
            public_key: G1Affine::from_miracl(&self.key_value),
            associated_data: associated_data.to_vec(),
        };
        verify_pop(&instance, &self.proof_data).is_ok()
    }
}

impl std::fmt::Debug for PublicKeyWithPop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "y: ")?;
        format_ecp(f, &self.key_value)?;
        write!(f, ", ...}}")
    }
}

/// NI-DKG system parameters
pub struct SysParam {
    pub lambda_t: usize,
    pub lambda_h: usize,
    pub f0: ECP2,       // f_0 in the paper.
    pub f: Vec<ECP2>,   // f_1, ..., f_{lambda_T} in the paper.
    pub f_h: Vec<ECP2>, // The remaining lambda_H f_i's in the paper.
    pub h: ECP2,
}

/// Generates a (public key, secret key) pair for of forward-secure
/// public-key encryption scheme.
///
/// # Arguments:
/// * `associated_data`: public information for the Proof of Possession of the
///   key.
/// * `sys`: system parameters for the FS Encryption scheme.
/// * `rng`: seeded pseudo random number generator.
pub fn kgen(
    associated_data: &[u8],
    sys: &SysParam,
    rng: &mut impl RAND,
) -> (PublicKeyWithPop, SecretKey) {
    let g1 = ECP::generator();
    let g2 = ECP2::generator();
    let spec_p = BIG::new_ints(&rom::CURVE_ORDER);
    // x <- getRandomZp
    // rho <- getRandomZp
    // let y = g1^x
    // let pk = (y, pi_dlog)
    // let dk = (g1^rho, g2^x * f0^rho, f1^rho, ..., f_lambda^rho, h^rho)
    // return (pk, dk)
    let spec_x = BIG::randomnum(&spec_p, rng);
    let rho = BIG::randomnum(&spec_p, rng);
    let a = g1.mul(&rho);
    let mut b = g2.mul(&spec_x);
    b.add(&sys.f0.mul(&rho));
    let mut d_t = LinkedList::new();
    for f in sys.f.iter() {
        d_t.push_back(f.mul(&rho));
    }
    let mut d_h = Vec::new();
    for f in sys.f_h.iter() {
        d_h.push(f.mul(&rho));
    }
    let e = sys.h.mul(&rho);
    let bte_root = BTENode {
        tau: Vec::new(),
        a,
        b,
        d_t,
        d_h,
        e,
    };
    let sk = SecretKey::new(bte_root);

    let y = g1.mul(&spec_x);

    let pop_instance = EncryptionKeyInstance {
        g1_gen: G1Affine::generator(),
        public_key: G1Affine::from_miracl(&y),
        associated_data: associated_data.to_vec(),
    };

    let pop = prove_pop(&pop_instance, &Scalar::from_miracl(&spec_x), rng)
        .expect("Implementation bug: Pop generation failed");

    (
        PublicKeyWithPop {
            key_value: y,
            proof_data: pop,
        },
        sk,
    )
}

impl SecretKey {
    /// The current key (the end of list of BTENodes) of a `SecretKey` should
    /// always correspond to an epoch described by lambda_t bits. Some
    /// internal operations break this invariant, leaving less than lambda_t
    /// bits in the current key. This function should be called when this
    /// happens; it modifies the list so the current key corresponds to the
    /// first epoch of the subtree described by the current key.
    ///
    /// For example, if lambda_t = 5, then [..., 011] will change to
    /// [..., 0111, 01101, 01100].
    /// The current key's `tau` now has 5 bits, and the other entries cover the
    /// rest of the 011 subtree after we delete the current key.
    ///
    /// Another example: during the very first epoch the private key is
    /// [1, 01, 001, 0001, 00001, 00000].
    ///
    /// This makes key update easy: pop off the current key, then call this
    /// function.
    ///
    /// An alternative is to only store the root nodes of the subtrees that
    /// cover the remaining valid keys. Thus the first epoch, the private
    /// key would simply be \[0\], and would only change to [1, 01, 001, 0001,
    /// 00001] after the first update. Generally, some computations
    /// happen one epoch later than they would with our current scheme. However,
    /// key update is a bit fiddlier.
    ///
    /// No-op if `self` is empty.
    pub(crate) fn fast_derive(&mut self, sys: &SysParam, rng: &mut impl RAND) {
        let mut epoch = Vec::new();
        if self.bte_nodes.is_empty() {
            return;
        }
        let now = self.current().expect("bte_nodes unexpectedly empty");
        for i in 0..sys.lambda_t {
            if i < now.tau.len() {
                epoch.push(now.tau[i]);
            } else {
                epoch.push(Bit::Zero);
            }
        }
        self.update_to(&epoch, sys, rng);
    }

    fn new(bte_root: BTENode) -> SecretKey {
        let mut bte_nodes = LinkedList::new();
        bte_nodes.push_back(bte_root);
        SecretKey { bte_nodes }
    }

    /// Returns this key's  BTE-node that corresponds to the current epoch.
    pub fn current(&self) -> Option<&BTENode> {
        self.bte_nodes.back()
    }

    /// Updates this key to the next epoch.  After an update,
    /// the decryption keys for previous epochs are not accessible any more.
    /// (KUpd(dk, 1) from Sect. 9.1)
    pub fn update(&mut self, sys: &SysParam, rng: &mut impl RAND) {
        self.fast_derive(sys, rng);
        match self.bte_nodes.pop_back() {
            None => {}
            Some(mut dk) => {
                dk.zeroize();
                self.fast_derive(sys, rng);
            }
        }
    }

    /// Updates `self` to the given `epoch`.
    ///
    /// If `epoch` is in the past, then disables `self`.
    pub fn update_to(&mut self, epoch: &[Bit], sys: &SysParam, rng: &mut impl RAND) {
        // dropWhileEnd (\node -> not $ tau node `isPrefixOf` epoch) bte_nodes
        loop {
            match self.bte_nodes.back() {
                None => return,
                Some(cur) => {
                    if is_prefix(&cur.tau, epoch) {
                        break;
                    }
                }
            }
            self.bte_nodes
                .pop_back()
                .expect("bte_nodes unexpectedly empty")
                .zeroize();
        }

        let g1 = ECP::generator();
        let spec_r = BIG::new_ints(&rom::CURVE_ORDER);
        // At this point, bte_nodes.back() is a prefix of `epoch`.
        // Replace it with the nodes for `epoch` and later (in the subtree).
        //
        // For example, with a 5-bit epoch, if `node` is 011, and `epoch` is
        // 01101, then we replace [..., 011] with [..., 0111, 01101]:
        //   * The current epoch is now 01101.
        //   * We can still derive the keys for 01110 and 01111 from 0111.
        //   * We can no longer decrypt 01100.
        let mut node = self.bte_nodes.pop_back().expect("self.bte_nodes was empty");
        let mut n = node.tau.len();
        // Nothing to do if `node.tau` is already `epoch`.
        if n == epoch.len() {
            self.bte_nodes.push_back(node);
            return;
        }
        let mut d_t = node.d_t.clone();
        // Accumulators.
        //   b_acc = b * product [d_i^tau_i | i <- [1..n]]
        //   f_acc = f0 * product [f_i^tau_i | i <- [1..n]]
        let mut b_acc = node.b.clone();
        let mut f_acc = ftau_partial(&node.tau, sys).expect("node.tau not the expected size");
        let mut tau = node.tau.clone();
        while n < epoch.len() {
            if epoch[n] == Bit::Zero {
                // Save the root of the right subtree for later.
                let mut tau_1 = tau.clone();
                tau_1.push(Bit::One);
                let delta = BIG::randomnum(&spec_r, rng);

                let mut a_blind = g1.mul(&delta);
                a_blind.add(&node.a);
                let mut b_blind = d_t.pop_front().expect("d_t not sufficiently large");
                b_blind.add(&b_acc);
                let mut ftmp = f_acc.clone();
                ftmp.add(&sys.f[n]);
                b_blind.add(&ftmp.mul(&delta));

                let mut e_blind = sys.h.mul(&delta);
                e_blind.add(&node.e);
                let mut d_t_blind = LinkedList::new();
                let mut k = n + 1;
                d_t.iter().for_each(|d| {
                    let mut tmp = sys.f[k].mul(&delta);
                    tmp.add(d);
                    d_t_blind.push_back(tmp);
                    k += 1;
                });
                let mut d_h_blind = Vec::new();
                node.d_h.iter().zip(&sys.f_h).for_each(|(d, f)| {
                    let mut tmp = f.mul(&delta);
                    tmp.add(d);
                    d_h_blind.push(tmp);
                });
                self.bte_nodes.push_back(BTENode {
                    tau: tau_1,
                    a: a_blind,
                    b: b_blind,
                    d_t: d_t_blind,
                    d_h: d_h_blind,
                    e: e_blind,
                });
            } else {
                // Update accumulators.
                f_acc.add(&sys.f[n]);
                b_acc.add(&d_t.pop_front().expect("d_t not sufficiently large"));
            }
            tau.push(epoch[n]);
            n += 1;
        }

        let delta = BIG::randomnum(&spec_r, rng);
        let mut a = g1.mul(&delta);
        a.add(&node.a);
        let mut e = sys.h.mul(&delta);
        e.add(&node.e);
        b_acc.add(&f_acc.mul(&delta));

        let mut d_t_blind = LinkedList::new();
        // Typically `d_t_blind` remains empty.
        // It is only nontrivial if `epoch` is less than LAMBDA_T bits.
        let mut k = n;
        d_t.iter().for_each(|d| {
            let mut tmp = sys.f[k].mul(&delta);
            tmp.add(d);
            d_t_blind.push_back(tmp);
            k += 1;
        });
        let mut d_h_blind = Vec::new();
        node.d_h.iter().zip(&sys.f_h).for_each(|(d, f)| {
            let mut tmp = f.mul(&delta);
            tmp.add(d);
            d_h_blind.push(tmp);
        });

        self.bte_nodes.push_back(BTENode {
            tau,
            a,
            b: b_acc,
            d_t: d_t_blind,
            d_h: d_h_blind,
            e,
        });
        node.zeroize();
    }
}

/// Forward secure ciphertexts
///
/// This is (C,R,S,Z) tuple of section 5.2, with multiple C values,
/// one for each recipent.
pub struct Crsz {
    pub cc: Vec<Vec<ECP>>,
    pub rr: Vec<ECP>,
    pub ss: Vec<ECP>,
    pub zz: Vec<ECP2>,
}

fn format_ecp(f: &mut std::fmt::Formatter<'_>, ecp: &ECP) -> std::fmt::Result {
    let mut ecp_buffer = [0; 49];
    ecp.tobytes(&mut ecp_buffer, true);
    write!(f, "0x{}", hex::encode(&ecp_buffer[..]))
}

impl std::fmt::Debug for Crsz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "CRSZ{{\n  cc: [")?;
        for ciphertext in &self.cc {
            writeln!(f, "    [")?;
            for chunk in ciphertext {
                write!(f, "      ")?;
                format_ecp(f, chunk)?;
                writeln!(f)?;
            }
            writeln!(f, "    ],")?;
        }
        write!(f, "  ], ... }}")
    }
}

/// Randomness needed for NIZK proofs.
pub struct ToxicWaste {
    pub spec_r: Vec<BIG>,
    pub(crate) s: Vec<BIG>,
}

impl zeroize::Zeroize for ToxicWaste {
    fn zeroize(&mut self) {
        self.spec_r.iter_mut().for_each(|big| big.zero());
        self.s.iter_mut().for_each(|big| big.zero());
    }
}

/// Encrypt chunks. Returns ciphertext as well as the random spec_r's and s's
/// chosen, for later use in NIZK proofs.
pub fn enc_chunks(
    sij: &[Vec<isize>],
    pks: Vec<&ECP>,
    tau: &[Bit],
    associated_data: &[u8],
    sys: &SysParam,
    rng: &mut impl RAND,
) -> Option<(Crsz, ToxicWaste)> {
    if sij.is_empty() {
        return None;
    }

    // do
    //   chunks <- headMay allChunks
    //   guard $ all (== chunks) allChunks

    let all_chunks: LinkedList<_> = sij.iter().map(Vec::len).collect();
    let chunks = *all_chunks.front().expect("sij was empty");
    for si in sij.iter() {
        if si.len() != chunks {
            return None; // Chunk lengths disagree.
        }
        for x in si.iter() {
            if *x < CHUNK_MIN || *x > CHUNK_MAX {
                return None; // Chunk out of range.
            }
        }
    }

    use miracl_core::bls12381::pair::g1mul;
    use miracl_core::bls12381::pair::g2mul;
    let g1 = ECP::generator();
    let p = BIG::new_ints(&rom::CURVE_ORDER);

    // do
    //   spec_r <- replicateM chunks getRandom
    //   s <- replicateM chunks getRandom
    //   let rr = (g1^) <$> spec_r
    //   let ss = (g1^) <$> s
    let mut spec_r = Vec::new();
    let mut s = Vec::new();
    let mut rr = Vec::new();
    let mut ss = Vec::new();
    for _j in 0..chunks {
        {
            let tmp = BIG::randomnum(&p, rng);
            spec_r.push(tmp);
            rr.push(g1mul(&g1, &tmp));
        }
        {
            let tmp = BIG::randomnum(&p, rng);
            s.push(tmp);
            ss.push(g1mul(&g1, &tmp));
        }
    }
    // [[pk^spec_r * g1^s | (spec_r, s) <- zip rs si] | (pk, si) <- zip pks sij]
    let cc: Vec<Vec<_>> = sij
        .iter()
        .zip(&pks)
        .map(|(sj, pk)| {
            sj.iter()
                .zip(&spec_r)
                .map(|(s, spec_r)| pk.mul2(spec_r, &g1, &BIG::new_int(*s)))
                .collect()
        })
        .collect();

    let extended_tau = extend_tau(&cc, &rr, &ss, tau, associated_data);
    let id = ftau(&extended_tau, sys).expect("extended_tau not the correct size");
    let mut zz = Vec::new();
    for j in 0..chunks {
        let mut tmp = g2mul(&id, &spec_r[j]);
        tmp.add(&g2mul(&sys.h, &s[j]));
        zz.push(tmp);
    }
    Some((Crsz { cc, rr, ss, zz }, ToxicWaste { spec_r, s }))
}

fn is_prefix(xs: &[Bit], ys: &[Bit]) -> bool {
    // isPrefix [] _ = True
    // isPrefix _ [] = False
    // isPrefix (x:xt) (y:yt) = x == y && isPrefix xt yt
    if xs.len() > ys.len() {
        return false;
    }
    for i in 0..xs.len() {
        if xs[i] != ys[i] {
            return false;
        }
    }
    true
}

fn find_prefix<'a>(dks: &'a SecretKey, tau: &[Bit]) -> Option<&'a BTENode> {
    for node in dks.bte_nodes.iter() {
        if is_prefix(&node.tau, tau) {
            return Some(node);
        }
    }
    None
}

/// Solves discrete log problem with baby-step giant-step.
///
/// Returns:
///   find (\x -> base^x == tgt) [lo..lo + range - 1]
///
/// using an O(sqrt(N)) approach rather than a naive O(N) search.
///
/// We call `reduce()` before every `tobytes()` because this algorithm requires
/// the same element to serialize identically every time. (MIRACL does not
/// automatically perform Montgomery reduction for serialization, so in general
/// x == y does not imply x.tobytes() == y.tobytes().)
///
/// We cut the exponent in half, that is, for a range of 2^46, we build a table
/// of size 2^23 then perform up to 2^23 FP12 multiplications and lookups.
/// Depending on the cost of CPU versus RAM, it may be better to split
/// differently.
pub fn baby_giant(tgt: &FP12, base: &FP12, lo: isize, range: isize) -> Option<isize> {
    if range <= 0 {
        return None;
    }
    use std::collections::HashMap;
    let mut babies = HashMap::new();
    let mut n = 0;
    let mut g = FP12::new();
    g.one();
    loop {
        if n * n >= range {
            break;
        }
        let mut bytes = vec![0; FP12_SIZE];
        g.reduce();
        g.tobytes(&mut bytes);
        babies.insert(bytes, n);
        g.mul(base);
        n += 1;
    }
    g.inverse();

    let mut t = *base;
    if lo >= 0 {
        t = t.pow(&BIG::new_int(lo));
        t.inverse();
    } else {
        t = t.pow(&BIG::new_int(-lo));
    }
    t.mul(tgt);

    let mut x = lo;
    loop {
        let mut bytes = vec![0; FP12_SIZE];
        t.reduce();
        t.tobytes(&mut bytes);
        if let Some(i) = babies.get(&bytes) {
            return Some(x + i);
        }
        t.mul(&g);
        x += n;
        if x >= lo + range {
            break;
        }
    }
    None
}

/// Error while decrypting
#[derive(Debug)]
pub enum DecErr {
    ExpiredKey,
    InvalidChunk,
}

/// Decrypt the i-th group of chunks.
///
/// Decrypting a message for a future epoch hardly costs more than a message for
/// a current epoch: at most lambda_t point additions.
///
/// Upgrading a key is expensive in comparison because we must compute new
/// subtree roots and re-"blind" them (the random deltas of the paper) to hide
/// ciphertexts from future keys. Each re-blinding costs at least lambda_h
/// (which is 256 in our system) point multiplications.
///
/// Caller must ensure i < n, where n = crsz.cc.len().
pub fn dec_chunks(
    dks: &SecretKey,
    i: usize,
    crsz: &Crsz,
    tau: &[Bit],
    associated_data: &[u8],
) -> Result<Vec<isize>, DecErr> {
    let extended_tau = extend_tau(&crsz.cc, &crsz.rr, &crsz.ss, tau, associated_data);
    let dk = match find_prefix(dks, tau) {
        None => return Err(DecErr::ExpiredKey),
        Some(node) => node,
    };
    let mut bneg = dk.b.clone();
    let mut l = dk.tau.len();
    for tmp in dk.d_t.iter() {
        if extended_tau[l] == Bit::One {
            bneg.add(tmp);
        }
        l += 1
    }
    for k in 0..LAMBDA_H {
        if extended_tau[LAMBDA_T + k] == Bit::One {
            bneg.add(&dk.d_h[k]);
        }
    }
    bneg.neg();
    let g1 = ECP::generator();
    let g2 = ECP2::generator();
    let mut eneg = dk.e.clone();
    eneg.neg();
    let cj = &crsz.cc[i];
    use miracl_core::bls12381::pair;

    // zipWith4 f cj rr ss zz where
    //   f c spec_r s z =
    //     ate(g2, c) * ate(bneg, spec_r) * ate(z, dk_a) * ate(eneg, s)
    let powers: Vec<_> = cj
        .iter()
        .zip(crsz.rr.iter().zip(crsz.ss.iter().zip(crsz.zz.iter())))
        .map(|(c, (spec_r, (s, z)))| {
            let mut m = pair::ate2(&g2, c, &bneg, spec_r);
            m.mul(&pair::ate2(z, &dk.a, &eneg, s));
            pair::fexp(&m)
        })
        .collect();

    // Find discrete log of powers with baby-step-giant-step.
    let base = pair::fexp(&pair::ate(&g2, &g1));
    let mut dlogs = Vec::new();
    let spec_n = crsz.cc.len();
    let spec_m = crsz.cc[0].len();
    for item in powers.iter() {
        match baby_giant(item, &base, 0, CHUNK_SIZE) {
            // Happy path: honest DKG participants.
            Some(dlog) => dlogs.push(BIG::new_int(dlog)),
            // It may take hours to brute force a cheater's discrete log.
            None => match solve_cheater_log(spec_n, spec_m, item) {
                Some(big) => dlogs.push(big),
                None => panic!("Unsolvable discrete log!"),
            },
        }
    }

    // Clippy dislikes `FrBytes::SIZE` or `MESSAGE_BYTES` instead of `32`.
    let mut fr_bytes = [0u8; 32];
    let mut big_bytes = [0u8; 48];
    let b = BIG::new_int(CHUNK_SIZE);
    let mut acc = BIG::new_int(0);
    let r = BIG::new_ints(&rom::CURVE_ORDER);
    for src in dlogs.iter() {
        acc = BIG::modadd(src, &BIG::modmul(&acc, &b, &r), &r);
    }
    acc.tobytes(&mut big_bytes);
    fr_bytes[..].clone_from_slice(&big_bytes[16..(32 + 16)]);

    // Break up fr_bytes into a vec of isize, which will be combined again later.
    // It may be better to simply return FrBytes and change enc_chunks() to take
    // FrBytes and have it break it into chunks. This would confine the chunking
    // logic to the DKG, where it belongs.
    // (I tried this for a while, but it seemed to touch a lot of code.)
    let redundant = fr_bytes[..]
        .chunks_exact(CHUNK_BYTES)
        .map(|x| 256 * (x[0] as isize) + (x[1] as isize))
        .collect();
    Ok(redundant)
}

// TODO(IDX-1866)
#[allow(clippy::result_unit_err)]
/// Verify ciphertext integrity
///
/// Part of DVfy of Section 7.1 of <https://eprint.iacr.org/2021/339.pdf>
//
/// In addition to verifying the proofs of chunking and sharing,
/// we must also verify ciphertext integrity.
pub fn verify_ciphertext_integrity(
    crsz: &Crsz,
    tau: &[Bit],
    associated_data: &[u8],
    sys: &SysParam,
) -> Result<(), ()> {
    let n = if crsz.cc.is_empty() {
        0
    } else {
        crsz.cc[0].len()
    };
    if crsz.rr.len() != n || crsz.ss.len() != n || crsz.zz.len() != n {
        // In theory, this is unreachable fail because deserialization only succeeds
        // when the vectors of a CRSZ have the same length. (In practice, it's
        // surprising how often "unreachable" code is reached!)
        return Err(());
    }

    let extended_tau = extend_tau(&crsz.cc, &crsz.rr, &crsz.ss, tau, associated_data);
    let id = ftau(&extended_tau, sys).expect("extended_tau not the correct size");

    let g1_neg = G1Affine::generator().neg();
    let precomp_id = G2Prepared::from(G2Affine::from_miracl(&id));

    // check for all j:
    //     1 =
    //      e(g1^{-1}, Z_j) *
    //      e(R_j, f_0 \Prod_{i=0}^{\lambda} f_i^{\tau_i}) *
    //      e(S_j,h)
    //   (PRECOMP_SYS_H is the system-parameter h value,
    //   pre-loaded before this function call)
    let checks: Result<(), ()> = crsz
        .rr
        .iter()
        .zip(crsz.ss.iter().zip(crsz.zz.iter()))
        .try_for_each(|(spec_r, (s, z))| {
            let spec_r = G1Affine::from_miracl(spec_r);
            let s = G1Affine::from_miracl(s);
            let z = G2Prepared::from(G2Affine::from_miracl(z));

            let v =
                Gt::multipairing(&[(&spec_r, &precomp_id), (&s, &PRECOMP_SYS_H), (&g1_neg, &z)]);

            if v.is_identity() {
                Ok(())
            } else {
                Err(())
            }
        });
    checks
}

/// Returns (tau || RO(cc, rr, ss, tau, associated_data)).
///
/// See the description of Deal in Section 7.1.
fn extend_tau(
    cc: &[Vec<ECP>],
    rr: &[ECP],
    ss: &[ECP],
    tau: &[Bit],
    associated_data: &[u8],
) -> Vec<Bit> {
    let mut map = HashedMap::new();
    map.insert_hashed("ciphertext-chunks", &cc.to_vec());
    map.insert_hashed("randomizers-r", &rr.to_vec());
    map.insert_hashed("randomizers-s", &ss.to_vec());
    map.insert_hashed("epoch", &(epoch_from_tau_vec(tau).get() as usize));
    map.insert_hashed("associated-data", &associated_data.to_vec());

    let hash = random_oracle(DOMAIN_CIPHERTEXT_NODE, &map);

    let mut extended_tau: Vec<Bit> = tau.to_vec();
    hash.iter().for_each(|byte| {
        for b in 0..8 {
            extended_tau.push(Bit::from((byte >> b) & 1));
        }
    });
    extended_tau
}

/// Computes the function f of the paper.
///
/// The bit vector tau must have length lambda_T + lambda_H.
fn ftau(tau: &[Bit], sys: &SysParam) -> Option<ECP2> {
    if tau.len() != sys.lambda_t + sys.lambda_h {
        return None;
    }
    let mut id = sys.f0.clone();
    for (n, t) in tau.iter().enumerate() {
        if *t == Bit::One {
            if n < sys.lambda_t {
                id.add(&sys.f[n]);
            } else {
                id.add(&sys.f_h[n - sys.lambda_t]);
            }
        }
    }
    Some(id)
}

/// Computes f for bit vectors tau <= lambda_T.
fn ftau_partial(tau: &[Bit], sys: &SysParam) -> Option<ECP2> {
    if tau.len() > sys.lambda_t {
        return None;
    }
    // id = product $ f0 : [f | (t, f) <- zip tau sys_fs, t == 1]
    let mut id = sys.f0.clone();
    tau.iter().zip(sys.f.iter()).for_each(|(t, f)| {
        if *t == Bit::One {
            id.add(f);
        }
    });
    Some(id)
}

// An FS key upgrade can take up to 2 * LAMBDA_T * LAMBDA_H point
// multiplications. This is tolerable in practice for LAMBDA_T = 32, but in
// tests, smaller values are preferable.

/// Constant which controls the upper limit of epochs
///
/// Specifically 2**LAMBDA_T NI-DKG epochs cann occur
///
/// See Section 7.1 of <https://eprint.iacr.org/2021/339.pdf>
pub const LAMBDA_T: usize = 32;

/// The size of the hash function used during encryption
///
/// See Section 7.1 of <https://eprint.iacr.org/2021/339.pdf>
const LAMBDA_H: usize = 256;

/// Return NI-DKG system parameters
pub fn mk_sys_params() -> SysParam {
    let dst = b"DFX01-with-BLS12381G2_XMD:SHA-256_SSWU_RO_";
    let f0 = G2Affine::hash(dst, b"f0").to_miracl();

    let mut f = Vec::with_capacity(LAMBDA_T);
    for i in 0..LAMBDA_T {
        let s = format!("f{}", i + 1);
        f.push(G2Affine::hash(dst, s.as_bytes()).to_miracl());
    }
    let mut f_h = Vec::with_capacity(LAMBDA_H);
    for i in 0..LAMBDA_H {
        let s = format!("f_h{}", i);
        f_h.push(G2Affine::hash(dst, s.as_bytes()).to_miracl());
    }

    let h = G2Affine::hash(dst, b"h").to_miracl();

    SysParam {
        lambda_t: LAMBDA_T,
        lambda_h: LAMBDA_H,
        f0,
        f,
        f_h,
        h,
    }
}

/// Create a BIG from an isize
///
/// Miracl's documentation cautions against using BIG to hold negative integers.
/// However, sometimes our code produces negative isize values representing
/// elements of Z_r (where r is the order of G1).
pub fn negative_safe_new_int(n: isize) -> BIG {
    if n < 0 {
        let mut tmp = BIG::new_int(-n);
        tmp.rsub(&curve_order());
        tmp
    } else {
        BIG::new_int(n)
    }
}

/// Brute-forces a discrete log for a malicious DKG participant whose NIZK
/// chunking proof checks out.
///
/// For some Delta in [1..E - 1] the answer s satisfies (Delta * s) in
/// [1 - Z..Z - 1].
pub fn solve_cheater_log(spec_n: usize, spec_m: usize, target: &FP12) -> Option<BIG> {
    use miracl_core::bls12381::pair;
    let bb_constant = CHUNK_SIZE as usize;
    let ee = 1 << CHALLENGE_BITS;
    let ss = spec_n * spec_m * (bb_constant - 1) * (ee - 1);
    let zz = (2 * NUM_ZK_REPETITIONS * ss) as isize;
    let base = pair::fexp(&pair::ate(&ECP2::generator(), &ECP::generator()));
    let mut target_power = FP12::new_int(1);
    let spec_r = BIG::new_ints(&rom::CURVE_ORDER);
    // For each Delta in [1..E - 1] we compute target^Delta and use
    // baby-step-giant-step to find `scaled_answer` such that:
    //   base^scaled_answer = target^Delta
    // Then base^(scaled_answer * invDelta) = target where
    //   invDelta = inverse of Delta mod spec_r
    // That is, answer = scaled_answer * invDelta.
    for delta in 1..ee {
        target_power.mul(target);
        match baby_giant(&target_power, &base, 1 - zz, 2 * zz - 1) {
            None => {}
            Some(scaled_answer) => {
                let mut answer = BIG::new_int(delta as isize);
                answer.invmodp(&spec_r);
                answer = BIG::modmul(&answer, &negative_safe_new_int(scaled_answer), &spec_r);
                answer.norm();
                return Some(answer);
            }
        }
    }
    None
}
