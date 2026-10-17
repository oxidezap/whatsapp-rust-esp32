/// Remove a validated NVS header/key without allocating a second payload.
/// The caller must validate the record and its logical key before passing the
/// payload offset. Retain the allocation (including its small prefix capacity)
/// rather than shrinking it and reintroducing an allocation/copy peak.
pub(super) fn remove_prefix(mut record: Vec<u8>, payload_start: usize) -> Vec<u8> {
    let payload_len = record.len() - payload_start;
    record.copy_within(payload_start.., 0);
    record.truncate(payload_len);
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_payload_keeps_all_bytes_and_reuses_the_original_allocation() {
        for key_len in [0, 1, 31, 127, 128, 511, 512] {
            for payload_len in [0, 1, 127, 128, 2969, 4096, 65536] {
                let payload: Vec<u8> = (0..payload_len).map(|i| (i % 256) as u8).collect();
                let prefix = 5 + key_len;
                let mut record = Vec::with_capacity(prefix + payload.len() + 17);
                record.extend_from_slice(b"WA1");
                record.extend_from_slice(&(key_len as u16).to_le_bytes());
                record.resize(prefix, 0xa5);
                record.extend_from_slice(&payload);
                let allocation = record.as_ptr();
                let capacity = record.capacity();
                let decoded = remove_prefix(record, prefix);
                assert_eq!(decoded, payload);
                assert_eq!(decoded.as_ptr(), allocation);
                assert_eq!(decoded.capacity(), capacity);
            }
        }
    }
}
