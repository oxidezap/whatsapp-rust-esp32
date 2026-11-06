//! NVS integration regressions. Test namespaces never overlap linked credentials.

use super::*;
use futures::executor::block_on;

/// Open isolated namespaces on the test device without implicit partition repair.
fn isolated_store() -> NvsStore {
    let result = unsafe { esp_idf_svc::sys::nvs_flash_init_partition(c"wa_store".as_ptr()) };
    assert_eq!(result, 0, "test partition must initialize without repair");
    let partition = EspCustomNvsPartition::take(DEFAULT_PARTITION_NAME).unwrap();
    let flash = FlashNamespaces {
        control: EspNvs::new(partition.clone(), "test-control", true).unwrap(),
        device: EspNvs::new(partition.clone(), "test-device", true).unwrap(),
        identities: EspNvs::new(partition.clone(), "test-identity", true).unwrap(),
        sessions: EspNvs::new(partition.clone(), "test-session", true).unwrap(),
        prekeys: EspNvs::new(partition.clone(), "test-prekey", true).unwrap(),
        signed_prekeys: EspNvs::new(partition.clone(), "test-signedpre", true).unwrap(),
        sender_keys: EspNvs::new(partition.clone(), "test-senderkey", true).unwrap(),
        sync_keys: EspNvs::new(partition, "test-synckey", true).unwrap(),
        validated_sessions: SessionSlots::default(),
    };
    NvsStore {
        inner: Mutex::new(StoreInner::default()),
        flash: FlashWorker::start(flash).unwrap(),
        operation: Mutex::new(()),
        accepting_writes: AtomicBool::new(true),
    }
}

/// Every erasure path must make the next write consult durable slots and limits.
#[test]
fn erased_sessions_recheck_flash_existence_and_capacity() {
    // Match the C3 executor's priority. A higher-priority NVS sender can
    // otherwise starve its caller during std's zero-capacity rendezvous.
    struct RestorePriority(esp_idf_svc::sys::UBaseType_t);
    impl Drop for RestorePriority {
        fn drop(&mut self) {
            unsafe { esp_idf_svc::sys::vTaskPrioritySet(core::ptr::null_mut(), self.0) };
        }
    }
    let _priority =
        RestorePriority(unsafe { esp_idf_svc::sys::uxTaskPriorityGet(core::ptr::null_mut()) });
    unsafe { esp_idf_svc::sys::vTaskPrioritySet(core::ptr::null_mut(), 5) };
    let store = isolated_store();
    for path in 0..6 {
        store.reset().unwrap();
        store
            .flash
            .run(|flash| {
                flash.put_record(&flash.sessions, b"cached", b"old", "session", 1)?;
                assert_eq!(
                    flash.load_record(&flash.sessions, b"cached", MAX_SIGNAL_RECORD_LEN)?,
                    Some(b"old".to_vec())
                );
                Ok(())
            })
            .unwrap();

        match path {
            0 => block_on(store.delete_session("cached")).unwrap(),
            1 => assert_eq!(store.clear_sessions().unwrap(), 1),
            2 => store.reset().unwrap(),
            3 => store.flash.run(FlashNamespaces::erase_signal).unwrap(),
            4 => store.flash.run(FlashNamespaces::erase_all).unwrap(),
            5 => store.flash.run(FlashNamespaces::complete_reset).unwrap(),
            _ => unreachable!(),
        }

        store
            .flash
            .run(move |flash| {
                assert!(
                    flash
                        .load_record(&flash.sessions, b"cached", MAX_SIGNAL_RECORD_LEN)?
                        .is_none(),
                    "erasure path {path} retained the flash record"
                );
                flash.put_record(&flash.sessions, b"other", b"new", "session", 1)?;
                let error = flash
                    .put_record(&flash.sessions, b"cached", b"replacement", "session", 1)
                    .expect_err("a stale cache must not bypass the durable capacity check");
                assert!(
                    error.to_string().contains("storage limit reached"),
                    "path {path}: {error}"
                );
                assert_eq!(record_count(&flash.sessions)?, 1);

                // A record reusing the same physical name must be decoded again.
                set_blob(
                    &flash.sessions,
                    &record_name(b"cached"),
                    &encode_record(b"collision", b"value")?,
                )?;
                let error = flash
                    .put_record(&flash.sessions, b"cached", b"replacement", "session", 2)
                    .expect_err("a stale cache must not bypass the flash collision check");
                assert!(
                    error.to_string().contains("NVS key collision"),
                    "path {path}: {error}"
                );
                Ok(())
            })
            .unwrap();
    }
    store.reset().unwrap();
}
