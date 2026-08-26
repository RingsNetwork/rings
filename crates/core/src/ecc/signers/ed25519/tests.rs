use super::*;

#[test]
fn test_verify_ed25519() {
    // test via phantom
    // const msg = "helloworld";
    // const encoded = new TextEncoder().encode(msg);
    // const signedMessage = await solana.request({
    //     method: "signMessage",
    //     params: {
    //     message: encoded,
    //     },
    // });
    // publicKey: "9z1ZTaGocNSAu3DSqGKR6Dqt214X4dXucVd6C53EgqBK"
    // signature: "2V1AR5byk4a4CkVmFRWU1TVs3ns2CGkuq6xgGju1huGQGq5hGkiHUDjEaJJaL2txfqCSGnQW55jUJpcjKFkZEKq"

    let msg = "helloworld";
    let signer = PublicKey::try_from_b58t("9z1ZTaGocNSAu3DSqGKR6Dqt214X4dXucVd6C53EgqBK").unwrap();
    let sig_b58 =
        "2V1AR5byk4a4CkVmFRWU1TVs3ns2CGkuq6xgGju1huGQGq5hGkiHUDjEaJJaL2txfqCSGnQW55jUJpcjKFkZEKq";
    let sig: Vec<u8> = base58::FromBase58::from_base58(sig_b58).unwrap();
    assert!(self::verify(
        msg.as_bytes(),
        &signer.address(),
        sig.as_slice(),
        &signer
    ))
}
