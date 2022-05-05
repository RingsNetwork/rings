use crate::ecc::{PublicKey, SecretKey};
use crate::err::Error;
use crate::err::Result;
use libsecp256k1::curve::Affine;
use libsecp256k1::curve::ECMultGenContext;
use libsecp256k1::curve::Field;
use libsecp256k1::curve::FieldStorage;
use libsecp256k1::curve::Jacobian;
use libsecp256k1::curve::Scalar;
use libsecp256k1::curve::AFFINE_G as G;

pub fn u8_to_u32(s: &Vec<u8>) -> Vec<u32> {
    s.chunks(4)
        .map(|x| {
            let mut pad = vec![0; 4 - x.len()];
            pad.extend(x);
            pad.try_into().unwrap()
        })
        .fold(vec![], |mut x: Vec<u32>, y: [u8; 4]| {
            x.push(u32::from_be_bytes(y));
            x
        })
}

pub fn u32_to_u8(v: &Vec<u32>) -> Vec<u8> {
    v.iter()
        .map(|a| a.to_be_bytes().into_iter().skip_while(|x| *x == 0))
        .fold(vec![], |mut x: Vec<u8>, y| {
            x.extend(y);
            x
        })
}

pub fn u32_to_u8_raw(v: &Vec<u32>) -> Vec<u8> {
    v.iter()
        .map(|a| a.to_be_bytes().into_iter())
        .fold(vec![], |mut x: Vec<u8>, y| {
            x.extend(y);
            x
        })
}

pub fn str_to_u32(s: &str) -> Vec<u32> {
    s.to_string()
        .as_bytes()
        .chunks(4)
        .map(|x| {
            let mut pad = vec![0; 4 - x.len()];
            pad.extend(x);
            pad.try_into().unwrap()
        })
        .fold(vec![], |mut x: Vec<u32>, y: [u8; 4]| {
            x.push(u32::from_be_bytes(y));
            x
        })
}

pub fn u32_to_str(v: &Vec<u32>) -> Result<String> {
    String::from_utf8(
        v.iter()
            .map(|a| a.to_be_bytes().into_iter().skip_while(|x| *x == 0))
            .fold(vec![], |mut x: Vec<u8>, y| {
                x.extend(y);
                x
            }),
    )
    .map_err(Error::Utf8Encoding)
}

pub fn str_to_field(s: &str) -> Vec<Field> {
    s.as_bytes()
        .chunks(31)
        .map(|x| {
            let mut pad = vec![0u8; 32 - x.len()];
            pad.extend(x);
            pad[0] = 255;
            pad.try_into().unwrap()
        })
        .fold(vec![], |mut x: Vec<Field>, y: [u8; 32]| {
            let v: [u32; 8] = u8_to_u32(&y.to_vec()).as_slice().try_into().unwrap();
            x.push(FieldStorage(v).into());
            x
        })
}

pub fn field_to_str(f: &Vec<Field>) -> Result<String> {
    String::from_utf8(
        f.iter()
            .map(|x| {
                let f: FieldStorage = (*x).into();
                let mut v = u32_to_u8(&f.0.to_vec());
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
            let f: FieldStorage = x.into();
            let mut v = u32_to_u8_raw(&f.0.to_vec());
            v[0] = a;
            assert_eq!(v.len(), 32);
            let t: Field = FieldStorage(u8_to_u32(&v.to_vec()).try_into().unwrap()).into();
            if !ec.set_xo_var(&t, t.is_odd()) {
                lift_x(&x, Some(a - 1))
            } else {
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

pub fn affine_to_str(a: &Vec<Affine>) -> Result<String> {
    field_to_str(&a.iter().map(|x| x.x).collect())
}

pub fn encrypt(s: &str, k: &PublicKey) -> Vec<(Affine, Affine)> {
    let random_sec: libsecp256k1::SecretKey = SecretKey::random().into();
    let pubkey: libsecp256k1::PublicKey = (*k).into();

    let random_sar: Scalar = random_sec.into();
    let h: Affine = pubkey.into();

    str_to_affine(s)
        .into_iter()
        .map(|c| {
            let cxt = ECMultGenContext::new_boxed();

            let mut shared_sec = Jacobian::from_ge(&h);
            cxt.ecmult_gen(&mut shared_sec, &random_sar);

            let mut c1 = Jacobian::from_ge(&G);
            cxt.ecmult_gen(&mut c1, &random_sar);

            let c2 = shared_sec.add_ge(&c);
            (Affine::from_gej(&c1), Affine::from_gej(&c2))
        })
        .collect()
}

pub fn decrypt(m: &Vec<(Affine, Affine)>, k: &SecretKey) -> Result<String> {
    let seckey: libsecp256k1::SecretKey = (*k).into();
    let sar: Scalar = seckey.into();
    let cxt = ECMultGenContext::new_boxed();
    affine_to_str(
        &m.iter()
            .map(|(c1, c2)| {
                let mut j_c1 = Jacobian::from_ge(c1);
                let j_c2 = Jacobian::from_ge(c2);
                cxt.ecmult_gen(&mut j_c1, &sar);
                let t = Affine::from_gej(&j_c2).neg();
                Affine::from_gej(&j_c2.add_ge(&t))
            })
            .collect(),
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use rand::{distributions::Alphanumeric, Rng};

    fn random(len: usize) -> String {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(len)
            .map(char::from)
            .collect()
    }

    #[test]
    fn test_string_to_u32() {
        let t = "";
        assert_eq!(str_to_u32(t).len(), 0);

        let t = "test";
        assert_eq!(str_to_u32(t), [1952805748]);
        assert_eq!(u32_to_str(&str_to_u32(t)).unwrap(), t);

        let t = "abc";
        assert_eq!(u32_to_str(&str_to_u32(t)).unwrap(), t);

        let t = "hello world, hello world";
        assert_eq!(u32_to_str(&str_to_u32(t)).unwrap(), t);
        let t: String = random(1024);
        assert_eq!(u32_to_str(&str_to_u32(&t)).unwrap(), t);

        let t: String = random(127);
        assert_eq!(u32_to_str(&str_to_u32(&t)).unwrap(), t);
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

    // #[test]
    // fn test_encrypt_decrypt() {
    //     let key = SecretKey::random();
    //     let pubkey = key.pubkey();
    //     let t: String = random(1024);
    //     assert_eq!(decrypt(&encrypt(&t, &pubkey), &key).unwrap(), t)
    // }
}
