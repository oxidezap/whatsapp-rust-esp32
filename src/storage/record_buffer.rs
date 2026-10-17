/// Remove a validated NVS header/key without allocating a second payload.
/// The caller must validate the record and its logical key before passing the
/// payload offset. Retain the allocation (including its small prefix capacity)
/// rather than shrinking it and reintroducing an allocation/copy peak.
pub(super) fn remove_prefix(mut record: Vec<u8>, payload_start: usize) -> Vec<u8> {
    remove_prefix_in_place(&mut record, payload_start);
    record
}

pub(super) fn remove_prefix_in_place(record: &mut Vec<u8>, payload_start: usize) {
    let payload_len = record.len() - payload_start;
    record.copy_within(payload_start.., 0);
    record.truncate(payload_len);
}

/// Lend unused capacity to a temporary read without touching the live payload.
/// Restore its length on success, an error returned by the callback, or unwind.
pub(super) fn with_suffix<T>(
    record: &mut Vec<u8>,
    length: usize,
    read: impl FnOnce(&mut [u8]) -> T,
) -> Result<T, std::collections::TryReserveError> {
    let start = record.len();
    record.try_reserve_exact(length)?;
    record.resize(start + length, 0);
    struct Restore<'a> {
        record: &'a mut Vec<u8>,
        length: usize,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.record.truncate(self.length);
        }
    }
    let restore = Restore {
        record,
        length: start,
    };
    Ok(read(&mut restore.record[start..]))
}

/// Frame an owned payload in its existing allocation. The caller validates
/// header/key limits and removes this prefix after the durable write.
pub(super) fn prepend_parts(
    record: &mut Vec<u8>,
    header: &[u8],
    key: &[u8],
) -> Result<usize, std::collections::TryReserveError> {
    let prefix = header.len() + key.len();
    record.try_reserve_exact(prefix)?;
    let payload = record.len();
    record.resize(payload + prefix, 0);
    record.copy_within(..payload, prefix);
    record[..header.len()].copy_from_slice(header);
    record[header.len()..prefix].copy_from_slice(key);
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_and_framing_share_reserved_storage_and_preserve_payload() {
        for payload_len in [0, 128, 2969, 3432, 4433] {
            let payload = vec![0x5a; payload_len];
            let mut buffer = Vec::with_capacity(8192);
            buffer.extend_from_slice(&payload);
            let pointer = buffer.as_ptr();
            let old = vec![0xa5; 2969];
            let result = with_suffix(&mut buffer, old.len(), |scratch| {
                scratch.copy_from_slice(&old);
                assert_eq!(scratch, old);
                Err::<(), _>("synthetic validation failure")
            })
            .unwrap();
            assert!(result.is_err());
            assert_eq!(buffer, payload);
            let header = *b"WA1\x03\x00";
            let prefix = prepend_parts(&mut buffer, &header, b"key").unwrap();
            assert_eq!(&buffer[..prefix], b"WA1\x03\x00key");
            assert_eq!(&buffer[prefix..], payload);
            remove_prefix_in_place(&mut buffer, prefix);
            assert_eq!(buffer, payload);
            assert_eq!(buffer.as_ptr(), pointer);
            assert_eq!(buffer.capacity(), 8192);
        }
    }

    #[test]
    fn failed_scratch_reservation_does_not_change_the_live_payload() {
        let mut buffer = vec![1, 2, 3];
        assert!(with_suffix(&mut buffer, usize::MAX, |_| panic!("must not run")).is_err());
        assert_eq!(buffer, [1, 2, 3]);
    }

    #[test]
    fn scratch_read_unwind_restores_the_live_payload_length() {
        let mut buffer = vec![1, 2, 3];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = with_suffix(&mut buffer, 5, |_| panic!("synthetic read panic"));
        }));
        assert!(result.is_err());
        assert_eq!(buffer, [1, 2, 3]);
    }

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
