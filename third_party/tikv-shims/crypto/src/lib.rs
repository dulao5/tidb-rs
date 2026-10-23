pub mod rand {
    use rand::RngCore;
    #[derive(Debug)]
    pub struct ErrorStack;
    impl std::fmt::Display for ErrorStack { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("rand error") } }
    impl std::error::Error for ErrorStack {}
    pub fn rand_bytes(buf: &mut [u8]) -> Result<(), ErrorStack> { rand::rngs::OsRng.fill_bytes(buf); Ok(()) }
    pub fn rand_u64() -> Result<u64, ErrorStack> { Ok(rand::rngs::OsRng.next_u64()) }
}
