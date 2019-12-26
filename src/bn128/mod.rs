// TODO: add test vectors from https://github.com/ethereum/go-ethereum/blob/7b189d6f1f7eedf46c6607901af291855b81112b/core/vm/contracts_test.go
// TODO: hash_to_curve https://tools.ietf.org/html/draft-irtf-cfrg-hash-to-curve-04#page-37
// TODO: BLS draft https://github.com/cfrg/draft-irtf-cfrg-bls-signature/blob/master/draft-irtf-cfrg-bls-signature-00.txt
//
// Some sources to fast hash to curve algorithms:
// https://gist.github.com/hermanjunge/3308fbd3627033fc8d8ca0dd50809844

use crate::BLS;

use bn::{arith, AffineG1, AffineG2, Fq, Fq2, Fr,  G1, G2, Group, Gt, pairing_batch};
use byteorder::{BigEndian, ByteOrder};
use digest::Digest;
use sha2;

/// Module containing error definitions
mod error;
use error::Error;

struct Bn128;

impl Bn128 {
    /// Function to convert an arbitrary string to a point in the curve
    ///
    /// # Arguments
    ///
    /// * `data` - A slice representing the data to be converted to a point.
    ///
    /// # Returns
    ///
    /// * If successful, a `G1` representing the converted point.
    fn arbitrary_string_to_point(&self, data: &[u8]) -> Result<G1, Error> {
        let mut v = vec![0x02];
        v.extend(data);
        let point = G1::from_compressed(&v)?;

        Ok(point)
    }

    /// Function to convert a `Hash(PK|DATA)` to a point in the curve as stated in [VRF-draft-05](https://tools.ietf.org/pdf/draft-irtf-cfrg-vrf-05)
    /// (section 5.4.1.1).
    ///
    /// Point multiplication by the cofactor is not required for curve `bn128`.
    /// Since this curve is of prime order, every non-identity point is a generator, therefore the cofactor is 1.
    ///
    /// # Arguments
    ///
    /// * `public_key` - A slice of `[u8]` representing the public key in compressed form.
    /// * `msg` - A slice containing the input data.
    ///
    /// # Returns
    ///
    /// * If successful, a point in the `G1` group representing the hashed point.
    fn hash_to_try_and_increment(&self, _public_key: &[u8], msg: &[u8]) -> Result<G1, Error> {
        let mut c = 0..255;

        // Add prefixes and counter suffix
        let cipher = [0xFF, 0x01];
        // let mut v = [&cipher[..], &public_key[..], &msg[..], &[0x00]].concat();
        let mut v = [&cipher[..], &msg[..], &[0x00]].concat();
        let position = v.len() - 1;

        // `Hash(cipher||PK||data)`
        let point = c.find_map(|ctr| {
            v[position] = ctr;
            let attempted_hash = self.calculate_sha256(&v);
            // Check validity of `H` (i.e. point exists in group G1)
            self.arbitrary_string_to_point(&attempted_hash).ok()
        });

        // Return error if no valid point was found
        point.ok_or(Error::HashToPointError)
    }

    /// Function to convert `G1` point into compressed form (`0x02` if Y is even and `0x03` if Y is odd)
    ///
    /// # Arguments
    ///
    /// * `point` - A `G1` point.
    ///
    /// # Returns
    ///
    /// * If successful, a `Vec<u8>` with the compressed `G1` point.
    pub fn to_compressed_g1(&self, point: G1) -> Result<Vec<u8>, Error> {
        // From Jacobian to Affine first!
        let affine_coords = AffineG1::from_jacobian(point).ok_or(Error::Unknown)?;
        // Get X coordinate
        let x = Fq::into_u256(affine_coords.x());
        // Get Y coordinate
        let y = Fq::into_u256(affine_coords.y());
        // Get parity of Y
        let parity = y.get_bit(0).ok_or(Error::Unknown)?;

        // Take x as big endian into slice
        let mut s = [0u8; 32];
        x.to_big_endian(&mut s)?;
        let mut result: Vec<u8> = Vec::new();
        // Push 0x02 or 0x03 depending on parity
        result.push(if parity { 3 } else { 2 });
        // Append x
        result.append(&mut s.to_vec());

        Ok(result)
    }

    /// Calculate the SHA256 hash
    pub fn calculate_sha256(&self, bytes: &[u8]) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        hasher.input(&bytes);
        let mut hash = [0; 32];
        hash.copy_from_slice(&hasher.result());

        hash
    }

}

pub struct PrivateKey {
    sk: bn::Fr,
}

pub struct PublicKey {
    pk: bn::G2,
}

impl PrivateKey {
    pub fn from_sk(sk: &Fr) -> PrivateKey {
        PrivateKey { sk: *sk }
    }

    pub fn to_public(&self) -> Result<PublicKey, Error> {
        Ok(PublicKey {
            pk: G2::one() * self.sk,
        })
    }
}

impl PublicKey {
    pub fn to_u512(&self, point: Fq2) -> arith::U512 {
        let c0: arith::U256 = (point.real()).into_u256();
        let c1: arith::U256 = (point.imaginary()).into_u256();

        arith::U512::new(&c1, &c0, &Fq::modulus())
    }

    pub fn from_compressed(bytes: &[u8]) -> Result<Self, Error> {
        let uncompressed = G2::from_compressed(&bytes)?;
        Ok(PublicKey { pk: uncompressed })
    }

    pub fn to_compressed(&self) -> Result<Vec<u8>, Error> {
        let modulus = Fq::modulus();
        // From Jacobian to Affine first!
        let affine_coords = AffineG2::from_jacobian(self.pk).ok_or(Error::Unknown)?;

        // Get X real coordinate
        let x_real = Fq::into_u256(affine_coords.x().real());

        // Get X imaginary coordinate
        let x_imaginary = Fq::into_u256(affine_coords.x().imaginary());

        // Get Y and get sign
        let y = affine_coords.y();
        let y_neg = -y;
        let sign: u8 = if self.to_u512(y) > self.to_u512(y_neg) {
            0x0b
        } else {
            0x0a
        };

        // To U512 and its compressed representation
        let compressed = arith::U512::new(&x_imaginary, &x_real, &modulus);

        // To slice
        let mut buf: [u8; 64] = [0; (4 * 16)];
        for (l, i) in (0..4).rev().zip((0..4).map(|i| i * 16)) {
            BigEndian::write_u128(&mut buf[i..], compressed.0[l]);
        }

        // Result = sign || compressed
        let mut result: Vec<u8> = Vec::new();
        result.push(sign);
        result.append(&mut buf.to_vec());
        Ok(result)
    }

}

impl BLS<&[u8], &[u8], &[u8]> for Bn128 {
    type Error = Error;

    // TODO: Add documentation -> public key in G2
    fn derive_public_key(&mut self, secret_key: &[u8]) -> Result<Vec<u8>, Error> {
        let scalar = Fr::from_slice(&secret_key[0..32])?;
        let key = PrivateKey::from_sk(&scalar);
        let public = key.to_public()?;

        // Jacobian to Affine
        //  let affine = AffineG2::from_jacobian(public.pk).ok_or(Error::Unknown)?;

        public.to_compressed()
    }

    // TODO: Add documentation
    fn sign(&mut self, secret_key: &[u8], msg: &[u8]) -> Result<Vec<u8>, Self::Error> {
        let public_key = self.derive_public_key(&secret_key)?;

        // 1. Hash_to_try_and_increment --> H(m) as point in G1 (only if it exists)
        let hash_point = self.hash_to_try_and_increment(&public_key, &msg)?;

        // 2. Multiply hash_point times secret_key --> Signature in G1
        let sk = Fr::from_slice(&secret_key)?;
        let signature = hash_point * sk;

        // 3. Return signature as compressed bytes
        self.to_compressed_g1(signature)
    }

    // TODO: Add documentation
    fn verify(
        &mut self,
        public_key: &[u8],
        signature: &[u8],
        msg: &[u8],
    ) -> Result<(), Self::Error> {
        let mut vals = Vec::new();
        // First pairing input: e(H(m), PubKey)
        let hash_point = self.hash_to_try_and_increment(&public_key, &msg)?;
        let public_key_point = G2::from_compressed(&public_key)?;
        vals.push((hash_point, public_key_point));
        // Second pairing input:  e(-Signature,G2::one())
        let signature_point = G1::from_compressed(&signature)?;
        vals.push((signature_point, -G2::one()));
        // Pairing batch with one negated point
        let mul = pairing_batch(&vals);
        if mul == Gt::one() {
            Ok(())
        } else {
            Err(Error::VerificationFailed)
        }
    }

    // TODO: Add documentation
    fn aggregate_public_keys(&mut self, public_keys: &[&[u8]]) -> Result<Vec<u8>, Self::Error> {
        let agg_public_key: Result<G2, Error> = public_keys.iter().try_fold(G2::zero(), |acc, &compressed| {
            let public_key= PublicKey::from_compressed(&compressed)?;

            Ok(acc + public_key.pk)
        });

        PublicKey{pk: agg_public_key?}.to_compressed()
    }

    // TODO: Add documentation
    fn aggregate_signatures(&mut self, signatures: &[&[u8]]) -> Result<Vec<u8>, Self::Error> {
        let agg_signatures: Result<G1, Error> = signatures.iter().try_fold(G1::zero(), |acc, &compressed| {
            let signature= G1::from_compressed(&compressed)?;

            Ok(acc + signature)
        });

        self.to_compressed_g1(agg_signatures?)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Test vectors taken from https://asecuritysite.com/encryption/go_bn256. The public keys in G2 are changed in order in the website, i.e., imaginary goes first.
    // In order to construct the test vectors we need to do the following
    // Get the modulus of Fq
    // Get the components (real, imaginary) of x and y
    // perform (imaginary*modulus) +  real
    // Compress with 0x0a or 0x0b depending on the value of y
    #[test]
    fn test_compressed_public_key_1() {
        let compressed = hex::decode("0a023aed31b5a9e486366ea9988b05dba469c6206e58361d9c065bbea7d928204a761efc6e4fa08ed227650134b52c7f7dd0463963e8a4bf21f4899fe5da7f984a").unwrap();
        let public_key = PublicKey::from_compressed(&compressed).unwrap();
        let compressed_again = public_key.to_compressed().unwrap();
        assert_eq!(compressed, compressed_again);
    }

    #[test]
    fn test_to_public_key_1() {
        let secret_key = hex::decode("1ab1126ff2e37c6e6eddea943ccb3a48f83b380b856424ee552e113595525565").unwrap();
        let mut curve = Bn128 {};
        let public_key = curve.derive_public_key(&secret_key).unwrap();
        let g2 = G2::from_compressed(
            &public_key
        ).unwrap();

        assert_eq!(g2.x(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("28fe26becbdc0384aa67bf734d08ec78ecc2330f0aa02ad9da00f56c37907f78").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("2cd080d897822a95a0fb103c54f06e9bf445f82f10fe37efce69ecb59514abc8").unwrap()).unwrap(),
                   )
        );
        assert_eq!(g2.y(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("237faeb0351a693a45d5d54aa9759f52a71d76edae2132616d6085a9b2228bf9").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("0f46bd1ef47552c3089604c65a3e7154e3976410be01149b60d5a41a6053e6c2").unwrap()).unwrap(),
                   )
        );
    }

    #[test]
    fn test_to_public_key_2() {
        let secret_key = hex::decode("2009da7287c158b126123c113d1c85241b6e3294dd75c643588630a8bc0f934c").unwrap();
        let mut curve = Bn128 {};
        let public_key = curve.derive_public_key(&secret_key).unwrap();
        let g2 = G2::from_compressed(
            &public_key
        ).unwrap();
        assert_eq!(g2.x(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("1cd5df38ed2f184b9830bfd3c2175d53c1455352307ead8cbd7c6201202f4aa8").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("02ce1c4241143cc61d82589c9439c6dd60f81fa6f029625d58bc0f2e25e4ce89").unwrap()).unwrap(),
                   )
        );
        assert_eq!(g2.y(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("0ba19ae3b5a298b398b3b9d410c7e48c4c8c63a1d6b95b098289fbe1503d00fb").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("2ec596e93402de0abc73ce741f37ed4984a0b59c96e20df8c9ea1c4e6ec04556").unwrap()).unwrap(),
                   )
        );
    }

    #[test]
    fn test_to_public_key_3() {
        let secret_key = hex::decode("26fb4d661491b0a623637a2c611e34b6641cdea1743bee94c17b67e5ef14a550").unwrap();
        let mut curve = Bn128 {};
        let public_key = curve.derive_public_key(&secret_key).unwrap();
        let g2 = G2::from_compressed(
            &public_key
        ).unwrap();
        assert_eq!(g2.x(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("077dfcf14e940b69bf88fa1ad99b6c7e1a1d6d2cb8813ac53383bf505a17f8ff").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("2d1a9b04a2c5674373353b5a25591292e69c37c0b84d9ef1c780a57bb98638e6").unwrap()).unwrap(),
                   )
        );
        assert_eq!(g2.y(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("2dc52f109b333c4125bccf55bc3a839ce57676514405656c79e577e231519273").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("2410eee842807d9325f22d087fa6bc79d9bbea07f5fa8c345e1e57b28ad54f84").unwrap()).unwrap(),
                   )
        );
    }

    #[test]
    fn test_to_public_key_4() {
        let secret_key = hex::decode("0f6b8785374476a3b3e4bde2c64dfb12964c81c7930d32367c8e318609387872").unwrap();
        let mut curve = Bn128 {};
        let public_key = curve.derive_public_key(&secret_key).unwrap();
        let g2 = G2::from_compressed(
            &public_key
        ).unwrap();
        assert_eq!(g2.x(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("270567a05b56b02e813281d554f46ce0c1b742b622652ef5a41d69afb6eb8338").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("1bab5671c5107de67fe06007dde240a84674c8ff13eeac6d64bad0caf2cfe53e").unwrap()).unwrap(),
                   )
        );
        assert_eq!(g2.y(),
                   Fq2::new(
                       Fq::from_slice(&hex::decode("0142f4e04fc1402e17ae7e624fd9bd15f1eae0a1d8eda4e26ab70fd4cd793338").unwrap()).unwrap(),
                       Fq::from_slice(&hex::decode("02b54a5deaaf86dc7f03d080c8373d62f03b3be06dac42b2d9426a8ebd0caf4a").unwrap()).unwrap(),
                   )
        );
    }

    /// Test for the `hash_to_try_and_increment` function with own test vector
    #[test]
    fn test_hash_to_try_and_increment_1() {
        let mut curve = Bn128 {};

        // Public key
        let secret_key =
            hex::decode("2009da7287c158b126123c113d1c85241b6e3294dd75c643588630a8bc0f934c")
                .unwrap();
        let public_key = curve.derive_public_key(&secret_key).unwrap();

        // Data to be hashed with TAI (ASCII "sample")
        let data = hex::decode("73616d706c65").unwrap();
        let hash_point = curve.hash_to_try_and_increment(&public_key, &data).unwrap();
        let hash_bytes = curve.to_compressed_g1(hash_point).unwrap();

        let expected_hash =
            hex::decode("021c4beaa17d30dd78c1a822cc75722490aa2292e145a408eea0b66a23486b8dd9")
                .unwrap();
        assert_eq!(hash_bytes, expected_hash);
    }

    /// Test for the `sign`` function with own test vector
    #[test]
    fn test_sign_1() {
        let mut bn128 = Bn128 {};

        // Inputs: secret key and message "sample" in ASCII
        let secret_key =
            hex::decode("2009da7287c158b126123c113d1c85241b6e3294dd75c643588630a8bc0f934c")
                .unwrap();
        let data = hex::decode("73616d706c65").unwrap();

        // Sign data with secret key
        let signature = bn128.sign(&secret_key, &data).unwrap();

        let expected_signature =
            hex::decode("02209a2c52479455ebc10f084db453215fc47b0067a76df11677c0ff82c0cb782a")
                .unwrap();

        assert_eq!(signature, expected_signature);
    }

    /// Test `verify` function with own signed message
    #[test]
    fn test_verify_signed_msg() {
        let mut bn128 = Bn128 {};

        // Public key
        let secret_key =
            hex::decode("2009da7287c158b126123c113d1c85241b6e3294dd75c643588630a8bc0f934c")
                .unwrap();
        let public_key = bn128.derive_public_key(&secret_key).unwrap();

        // Signature
        let signature =
            hex::decode("02209a2c52479455ebc10f084db453215fc47b0067a76df11677c0ff82c0cb782a")
                .unwrap();

        // Message signed
        let msg = hex::decode("73616d706c65").unwrap();

        // Verify signature
        assert!(bn128.verify(&public_key, &signature, &msg).is_ok(), "Verification failed");
    }

    /// Test `aggregate_public_keys`
    #[test]
    fn test_aggregate_public_keys_1() {
        let mut bn128 = Bn128 {};

        // Public keys
        let public_key_1 = PublicKey{pk: G2::one()}.to_compressed().unwrap();
        let public_key_2 = PublicKey{pk: G2::one()}.to_compressed().unwrap();
        let public_keys = [&public_key_1[..], &public_key_2[..]];

        // Aggregation
        let agg_public_key =bn128.aggregate_public_keys(&public_keys).unwrap();

        // Check
        let expected = hex::decode("0b061848379c6bccd9e821e63ff6932738835b78e1e10079a0866073eba5b8bb444afbb053d16542e2b839477434966e5a9099093b6b3351f84ac19fe28f096548").unwrap();
        assert_eq!(agg_public_key, expected);
    }

    /// Test `aggregate_signatures`
    #[test]
    fn test_aggregate_signatures_1() {
        let mut bn128 = Bn128 {};

        // Signatures (as valid points on G1)
        let sign_1 = bn128.to_compressed_g1(G1::one()).unwrap();
        let sign_2 = bn128.to_compressed_g1(G1::one()).unwrap();
        let signatures = [&sign_1[..], &sign_2[..]];

        // Aggregation
        let agg_signature =bn128.aggregate_signatures(&signatures).expect("Signature aggregation should not fail if G1 points are valid.");

        // Check
        let expected = hex::decode("02030644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd3").unwrap();
        assert_eq!(agg_signature, expected);
    }

    /// Test aggregated signatures verification
    #[test]
    fn test_verify_aggregated_signatures_1() {
        let mut bn128 = Bn128 {};

        // Message
        let msg = hex::decode("73616d706c65").unwrap();

        // Signature 1
        let secret_key1 = hex::decode("1ab1126ff2e37c6e6eddea943ccb3a48f83b380b856424ee552e113595525565").unwrap();
        let public_key1 = bn128.derive_public_key(&secret_key1).unwrap();
        let sign_1 = bn128.sign(&secret_key1, &msg).unwrap();

        // Signature 2
        let secret_key2 = hex::decode("2009da7287c158b126123c113d1c85241b6e3294dd75c643588630a8bc0f934c").unwrap();
        let public_key2 = bn128.derive_public_key(&secret_key2).unwrap();
        let sign_2 = bn128.sign(&secret_key2, &msg).unwrap();

        // Public Key and Signature aggregation
        let agg_public_key =bn128.aggregate_public_keys(&[&public_key1, &public_key2]).unwrap();
        let agg_signature =bn128.aggregate_signatures(&[&sign_1, &sign_2]).unwrap();

        // Verification single signatures
        assert!(bn128.verify(&public_key1, &sign_1, &msg).is_ok(), "Signature 1 verification failed");
        assert!(bn128.verify(&public_key2, &sign_2, &msg).is_ok(), "Signature 2 signature verification failed");

        // Aggregated signature verification
        assert!(bn128.verify(&agg_public_key, &agg_signature, &msg).is_ok(), "Aggregated signature verification failed");
    }

//    /// Test `aggregate_public_keys`
//    #[test]
//    fn test_aggregate_signatures_1() {
//        let mut bn128 = Bn128 {};
//
//        let file = File::open("./src/bn256.json").expect("File should open read only");
//        let json: Value = serde_json::from_reader(file).expect("File should be proper JSON");
//        let adds = json["add"].as_array().expect("File should have priv key");
//        for (i, elem) in adds.iter().enumerate() {
//            println!("Test number {}:", i);
//            // Signatures (points in G1)
//            let x1 = hex::decode(elem.get("x1").unwrap().as_str().unwrap()).unwrap();
//            let y1 = hex::decode(elem.get("y1").unwrap().as_str().unwrap()).unwrap();
//            let p1x = Fq::from_slice(&x1).unwrap();
//            let p1y = Fq::from_slice(&y1).unwrap();
//            let p1 = G1::from(AffineG1::new(p1x, p1y).unwrap());
//            let sign1 = bn128.to_compressed_g1(p1).unwrap();
//
//            let x2 = hex::decode(elem.get("x2").unwrap().as_str().unwrap()).unwrap();
//            let y2 = hex::decode(elem.get("y2").unwrap().as_str().unwrap()).unwrap();
//            let p2x = Fq::from_slice(&x2).unwrap();
//            let p2y = Fq::from_slice(&y2).unwrap();
//            let p2 = G1::from(AffineG1::new(p2x, p2y).unwrap());
//            let sign2 = bn128.to_compressed_g1(p2).unwrap();
//
//            let signatures = [&sign1[..], &sign2];
//
//            // Expected aggregation
//            let expected_uncompressed = hex::decode(elem.get("result").unwrap().as_str().unwrap()).unwrap();
//            let mut expected: Vec<u8> = Vec::new();
//            expected.push(if &expected_uncompressed[63] % 2 == 0 {2} else {3} );
//            expected.extend_from_slice(&expected_uncompressed[0..32]);
//
//            // Check
//            let agg_signatures = bn128.aggregate_signatures(&signatures).expect("Builtin should not fail");
//
//            assert_eq!(agg_signatures, expected);
//        };
//    }
}
