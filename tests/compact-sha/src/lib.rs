#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256, Sha512};
    fn hex(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
    #[test]
    fn fips_abc() {
        assert_eq!(hex(Sha256::digest(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(Sha512::digest(b"abc")), "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
    }
    #[test]
    fn fips_million_a_streamed() {
        let mut sha256 = Sha256::new();
        let mut sha512 = Sha512::new();
        for _ in 0..1000 { sha256.update([b'a'; 1000]); sha512.update([b'a'; 1000]); }
        assert_eq!(hex(sha256.finalize()), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        assert_eq!(hex(sha512.finalize()), "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b");
    }
}
