pub fn calc_crc32_bytes(contents: &[u8]) -> u32 {
    let mut d = crc32fast::Hasher::new();
    d.update(contents);
    d.finalize()
}
