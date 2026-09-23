//! Equality with the AEZ v5 reference implementation on the committed known-answer vectors
//! (`vectors/reference.txt`, emitted by `vectors/generate.c` over the official `ref/` code).

use super::witness;
use crate::Aez;
use crate::DecryptError;
use crate::Tweak;
use crate::KEY_BYTES;

/// The fixture: one vector per line, `key nonce ad tau message ciphertext`.
const REFERENCE: &str = include_str!("../../vectors/reference.txt");

/// Number of vectors the generator emits: 2·161 sweep + 5·4·26 grid + 33·33 genkat.
const REFERENCE_COUNT: usize = 322 + 520 + 1089;

/// One known answer.
struct Vector {
    key: [u8; KEY_BYTES],
    /// `(N, A_1, …)`.
    components: Vec<Vec<u8>>,
    expansion: usize,
    message: Vec<u8>,
    ciphertext: Vec<u8>,
}

impl Vector {
    /// Parses one fixture line; `-` is the empty string.
    fn parse(line: &str) -> Self {
        let fields: Vec<&str> = line.split(' ').collect();
        let [key, nonce, associated, expansion, message, ciphertext] = fields.as_slice() else {
            panic!("malformed vector line: {line}");
        };
        let mut associated = associated.split(':');
        let count: usize = associated.next().unwrap().parse().unwrap();
        let components: Vec<Vec<u8>> = core::iter::once(unhex(nonce))
            .chain(associated.map(unhex))
            .collect();
        assert_eq!(components.len(), count + 1, "{line}");
        Self {
            key: unhex(key).try_into().unwrap(),
            components,
            expansion: expansion.parse().unwrap(),
            message: unhex(message),
            ciphertext: unhex(ciphertext),
        }
    }

    /// Runs `f` with the vector's key and tweak.
    fn with<R>(&self, f: impl FnOnce(&Aez, Tweak<'_>) -> R) -> R {
        let components: Vec<&[u8]> = self.components.iter().map(Vec::as_slice).collect();
        f(&Aez::new(&self.key).unwrap(), Tweak::new(&components))
    }
}

/// Hex decoding for the fixture.
fn unhex(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

/// All fixture vectors.
fn reference() -> Vec<Vector> {
    let vectors: Vec<Vector> = REFERENCE.lines().map(Vector::parse).collect();
    assert_eq!(vectors.len(), REFERENCE_COUNT);
    vectors
}

witness! {
    /// Law: `encrypt = Encrypt_ref` and `decrypt = Decrypt_ref` on every vector, for any
    /// initial slot content.
    fn encrypt_and_decrypt_agree_with_the_reference_code() {
        for (index, vector) in reference().iter().enumerate() {
            vector.with(|aez, tweak| {
                let mut buffer = vector.message.clone();
                buffer.resize(vector.message.len() + vector.expansion, 0xa5);
                aez.encrypt(tweak, vector.expansion, &mut buffer).unwrap();
                assert_eq!(buffer, vector.ciphertext, "encrypt, vector {index}");
                let plaintext = aez.decrypt(tweak, vector.expansion, &mut buffer).unwrap();
                assert_eq!(plaintext, vector.message.as_slice(), "decrypt, vector {index}");
            });
        }
    }

    /// Law: at `τ ≥ 4` every sampled single-bit change of a reference ciphertext is rejected,
    /// and the rejected buffer is zeroized. (At `τ = 1` a random change passes with
    /// probability `2^−8` by design, so those vectors are excluded.)
    fn single_bit_changes_are_rejected_and_zeroized() {
        for (index, vector) in reference().iter().enumerate().filter(|(_, v)| v.expansion >= 4) {
            let stride = vector.ciphertext.len().div_ceil(8).max(1);
            for position in (0..vector.ciphertext.len()).step_by(stride) {
                vector.with(|aez, tweak| {
                    let mut buffer = vector.ciphertext.clone();
                    buffer[position] ^= 1 << (position % 8);
                    let result = aez.decrypt(tweak, vector.expansion, &mut buffer).map(|_| ());
                    assert_eq!(result, Err(DecryptError::Inauthentic), "vector {index} at {position}");
                    assert!(buffer.iter().all(|byte| *byte == 0), "vector {index} not zeroized");
                });
            }
        }
    }
}
