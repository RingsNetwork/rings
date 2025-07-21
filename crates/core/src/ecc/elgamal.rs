//! Elgamal Crypto Implementation
//! ----------------
//! Algorithm Description
//! # Encrypt
//! A second party, Bob, encrypts a message 𝑀 to Alice under her public key (𝐺,𝑞,𝑔,ℎ)
//! as follows:
//!    Map the message 𝑀 to an element 𝑚 of 𝐺 using a reversible mapping function.
//! Choose an integer 𝑦
//! randomly from {1,…,𝑞−1}
//! 1. Compute 𝑠:=ℎ𝑦 This is called the shared secret.
//! 2. Compute 𝑐1:=𝑔𝑦
//! 3. Compute 𝑐2:=𝑚⋅𝑠
//! 4. Bob sends the ciphertext (𝑐1,𝑐2) to Alice.
//!
//! # Decrypt
//! Alice decrypts a ciphertext 𝑐1,𝑐2 with her private key 𝑠𝑘 as follows:
//! 1. Compute 𝑠:=𝑐𝑥1
//! 2. Compute 𝑠−1, the inverse of 𝑠 in the group 𝐺
//! 3. Compute 𝑚:=𝑐2⋅𝑠−1
//!
//! ref:
//!    T. ElGamal. A Public Key Cryptosystem and a Signature Scheme Based on Discrete Logarithms. IEEE Trans. Info. Theory, IT 31:469–472, 1985.
//!    ElGamal encryption <https://en.wikipedia.org/wiki/ElGamal_encryption>
//!    <http://www.docsdrive.com/pdfs/ansinet/itj/2005/299-306.pdf>
use libsecp256k1::curve::Affine;
use libsecp256k1::curve::ECMultContext;
use libsecp256k1::curve::ECMultGenContext;
use libsecp256k1::curve::Field;
use libsecp256k1::curve::Jacobian;
use libsecp256k1::curve::Scalar;

use crate::ecc::CurveEle;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

pub fn str_to_field(s: &str) -> Vec<Field> {
    s.as_bytes()
        .chunks(31)
        .map(|x| {
            let mut pad = vec![0u8; 32 - x.len()];
            let mut field = Field::default();
            pad.extend(x);
            pad[0] = 255;
            let data: [u8; 32] = pad.try_into().unwrap();
            assert!(field.set_b32(&data));
            field
        })
        .collect()
}

pub fn field_to_str(f: &[Field]) -> Result<String> {
    String::from_utf8(
        f.iter()
            .map(|x| {
                let mut field = *x;
                field.normalize();
                let mut v = field.b32();
                println!("{v:?}");
                if v.len() > 1 {
                    v[0] = 0u8;
                }
                let inner = v.into_iter();
                inner.skip_while(|n| *n == 0u8)
            })
            .fold(vec![], |mut x: Vec<u8>, y| {
                x.extend(y);
                x
            }),
    )
    .map_err(Error::Utf8Encoding)
}

fn lift_x(x: &Field, bias: Option<u8>) -> Affine {
    let mut ec = Affine::default();
    let mut x = *x;
    x.normalize();
    match bias {
        None => {
            if !ec.set_xo_var(&x, x.is_odd()) {
                lift_x(&x, Some(254))
            } else {
                ec
            }
        }
        Some(0) => {
            panic!("failed to lift x");
        }
        Some(a) => {
            let mut v = x.b32();
            let mut x = Field::default();
            v[0] = a;
            assert_eq!(v.len(), 32);
            assert!(x.set_b32(&v));
            x.normalize();
            if !ec.set_xo_var(&x, x.is_odd()) {
                lift_x(&x, Some(a - 1))
            } else {
                ec.x.normalize();
                ec.y.normalize();
                ec
            }
        }
    }
}

pub fn str_to_affine(s: &str) -> Vec<Affine> {
    str_to_field(s)
        .into_iter()
        .map(|a| lift_x(&a, None))
        .collect::<Vec<Affine>>()
}

pub fn affine_to_str(a: &[Affine]) -> Result<String> {
    field_to_str(a.iter().map(|x| x.x).collect::<Vec<Field>>().as_slice())
}

pub fn encrypt(s: &str, k: PublicKey<33>) -> Result<Vec<(CurveEle<33>, CurveEle<33>)>> {
    let random_sar: Scalar = SecretKey::random().into();
    let mut h: Affine = k.try_into()?;
    h.y.normalize();
    h.y.normalize();
    let affines: Vec<(Affine, Affine)> = str_to_affine(s)
        .into_iter()
        .map(|c| {
            let g_cxt = ECMultGenContext::new_boxed();
            let cxt = ECMultContext::new_boxed();

            let mut shared_sec = Jacobian::default();
            cxt.ecmult_const(&mut shared_sec, &h, &random_sar);

            let mut c1 = Jacobian::default();
            g_cxt.ecmult_gen(&mut c1, &random_sar);
            let mut a_c1 = Affine::from_gej(&c1);
            a_c1.x.normalize();
            a_c1.y.normalize();
            let c2 = shared_sec.add_ge(&c);
            let mut a_c2 = Affine::from_gej(&c2);
            a_c2.x.normalize();
            a_c2.y.normalize();
            (a_c1, a_c2)
        })
        .collect();
    let mut ret: Vec<(CurveEle<33>, CurveEle<33>)> = vec![];
    for (c1, c2) in affines {
        ret.push((c1.try_into()?, c2.try_into()?))
    }
    Ok(ret)
}

pub fn decrypt(m: &[(CurveEle<33>, CurveEle<33>)], k: SecretKey) -> Result<String> {
    let sar: Scalar = k.into();
    let cxt = ECMultContext::new_boxed();
    affine_to_str(
        m.iter()
            .map(|(c1, c2)| {
                let c1: Affine = (*c1).try_into().expect("bad curve point");
                let c2: Affine = (*c2).try_into().expect("bad curve point");
                let mut t = Jacobian::default();
                cxt.ecmult_const(&mut t, &c1, &sar);
                let a_t = Affine::from_gej(&t).neg();
                let j_c2 = Jacobian::from_ge(&c2);
                let mut ret = Affine::from_gej(&j_c2.add_ge(&a_t));
                ret.x.normalize();
                ret.y.normalize();
                println!("{ret:?}");
                ret
            })
            .collect::<Vec<Affine>>()
            .as_slice(),
    )
}

#[cfg(test)]
mod test {
    use rand::distributions::Alphanumeric;
    use rand::Rng;

    use super::*;

    fn random(len: usize) -> String {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(len)
            .map(char::from)
            .collect()
    }

    #[test]
    fn test_string_to_field() {
        let t: String = random(1024);
        assert_eq!(field_to_str(&str_to_field(&t)).unwrap(), t);

        let t: String = random(127);
        assert_eq!(field_to_str(&str_to_field(&t)).unwrap(), t);
    }

    #[test]
    fn test_string_to_affine() {
        let t: String = random(1024);
        assert_eq!(affine_to_str(&str_to_affine(&t)).unwrap(), t);

        let t: String = random(127);
        assert_eq!(affine_to_str(&str_to_affine(&t)).unwrap(), t);
    }

    #[test]
    fn test_algorithm() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let sec_key: libsecp256k1::SecretKey = key.into();
        let pubkey: libsecp256k1::PublicKey = key.pubkey().try_into().unwrap();
        let mut pub_point: Affine = pubkey.into();
        pub_point.x.normalize();
        pub_point.y.normalize();
        let pub_x = [
            226, 15, 49, 60, 133, 119, 254, 51, 180, 4, 209, 133, 17, 253, 134, 129, 149, 245, 53,
            173, 45, 62, 36, 113, 168, 153, 24, 91, 137, 141, 81, 47,
        ];
        let pub_y = [
            108, 113, 105, 68, 84, 69, 224, 17, 240, 33, 13, 214, 109, 90, 19, 142, 61, 78, 77,
            105, 96, 121, 193, 87, 117, 185, 180, 47, 202, 81, 181, 204,
        ];
        assert_eq!(pub_point.x.b32(), pub_x);
        assert_eq!(pub_point.y.b32(), pub_y);
        let test = "test";
        let points = str_to_affine(test);
        assert_eq!(points.len(), 1);
        assert_eq!(affine_to_str(&str_to_affine(test)).unwrap(), test);
        let m_point = points[0];
        let r: libsecp256k1::SecretKey =
            SecretKey::try_from("1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54")
                .unwrap()
                .into();
        let r_v = [
            31, 146, 117, 219, 175, 223, 186, 129, 148, 46, 179, 51, 11, 7, 243, 140, 190, 228,
            235, 184, 107, 220, 33, 116, 175, 150, 72, 213, 245, 80, 154, 84,
        ];
        let r_sca: Scalar = r.into();
        assert_eq!(r_sca.b32(), r_v);
        let cxt = ECMultGenContext::new_boxed();
        let mut c1 = Jacobian::default();
        cxt.ecmult_gen(&mut c1, &r_sca);
        let mut a_c1 = Affine::from_gej(&c1);

        a_c1.x.normalize();
        a_c1.y.normalize();
        let c1_x = [
            252, 168, 85, 233, 220, 119, 76, 217, 52, 108, 167, 27, 234, 188, 197, 95, 72, 213,
            148, 212, 111, 255, 6, 59, 9, 134, 111, 121, 175, 9, 189, 105,
        ];
        let c1_y = [
            20, 45, 13, 61, 245, 50, 136, 183, 182, 210, 169, 120, 84, 204, 77, 138, 12, 116, 50,
            9, 115, 98, 138, 245, 24, 61, 223, 144, 55, 180, 231, 59,
        ];
        assert_eq!(a_c1.x.b32(), c1_x);
        assert_eq!(a_c1.y.b32(), c1_y);

        let mut shared_sec = Jacobian::default();
        let cxt2 = ECMultContext::new_boxed();
        cxt2.ecmult_const(&mut shared_sec, &pub_point, &r_sca);
        let mut a_ss = Affine::from_gej(&shared_sec);
        a_ss.x.normalize();
        a_ss.y.normalize();

        let ss_x = [
            218, 19, 55, 137, 15, 46, 160, 160, 208, 222, 206, 77, 46, 79, 32, 80, 64, 243, 93, 23,
            223, 130, 148, 226, 131, 17, 254, 95, 43, 95, 35, 34,
        ];

        let ss_y = [
            106, 127, 47, 58, 214, 6, 110, 28, 171, 176, 73, 11, 34, 28, 125, 10, 82, 154, 84, 154,
            11, 80, 191, 68, 111, 197, 98, 224, 84, 116, 208, 115,
        ];
        assert_eq!(a_ss.x.b32(), ss_x);
        assert_eq!(a_ss.y.b32(), ss_y);
        let c2 = shared_sec.add_ge(&m_point);
        let c2_y = [
            77, 137, 184, 168, 131, 81, 80, 241, 75, 201, 50, 228, 133, 216, 144, 183, 20, 4, 156,
            185, 165, 63, 138, 39, 67, 207, 59, 130, 148, 102, 1, 250,
        ];
        let c2_x = [
            72, 198, 149, 212, 20, 110, 91, 144, 61, 150, 112, 138, 202, 132, 129, 174, 26, 223,
            251, 131, 211, 249, 93, 221, 218, 228, 231, 231, 201, 248, 14, 132,
        ];
        let mut a_c2 = Affine::from_gej(&c2);
        a_c2.x.normalize();
        a_c2.y.normalize();
        assert_eq!(a_c2.x.b32(), c2_x);
        assert_eq!(a_c2.y.b32(), c2_y);

        // decrypt
        let mut t = Jacobian::default();
        cxt2.ecmult_const(&mut t, &a_c1, &sec_key.into());
        let mut a_t = Affine::from_gej(&t);
        let t_x = [
            218, 19, 55, 137, 15, 46, 160, 160, 208, 222, 206, 77, 46, 79, 32, 80, 64, 243, 93, 23,
            223, 130, 148, 226, 131, 17, 254, 95, 43, 95, 35, 34,
        ];
        let t_y = [
            106, 127, 47, 58, 214, 6, 110, 28, 171, 176, 73, 11, 34, 28, 125, 10, 82, 154, 84, 154,
            11, 80, 191, 68, 111, 197, 98, 224, 84, 116, 208, 115,
        ];
        a_t.x.normalize();
        a_t.y.normalize();
        assert_eq!(a_t.x.b32(), t_x);
        assert_eq!(a_t.y.b32(), t_y);

        let ret = c2.add_ge(&a_t.neg());
        let mut a_ret = Affine::from_gej(&ret);
        a_ret.x.normalize();
        a_ret.y.normalize();
        assert_eq!(a_ret.x, m_point.x);
    }

    #[test]
    fn test_encrypt_decrypt() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let t: String = random(1024);
        assert_eq!(decrypt(&encrypt(&t, pubkey).unwrap(), key).unwrap(), t)
    }
}
