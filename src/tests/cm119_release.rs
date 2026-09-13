use super::*;

fn mixer_element(name: &str, capture: bool, playback: bool) -> FakeMixerElement {
    FakeMixerElement::new(
        name,
        0,
        FakeMixerCapabilities {
            capture_volume: capture,
            playback_volume: playback,
            capture_switch: capture,
            playback_switch: playback,
            capture_channels: [capture, false],
            playback_channels: [playback, false],
        },
    )
}

fn assert_unpublished(paths: &Cm119MixerPaths) {
    assert_eq!(paths.struct_size, size_of::<Cm119MixerPaths>() as u32);
    assert_eq!(paths.abi_version, 0);
    assert_eq!(paths.rx_capture_path_count, 0);
    assert_eq!(paths.tx_playback_path_count, 0);
    assert_eq!(paths.sidetone_path_count, 0);
    assert_eq!(paths.rx_compatibility_switch_path_count, 0);
}

#[test]
fn cm119_append_keeps_empty_capabilities_and_rejects_unrepresentable_paths() {
    let mut paths = [Cm119MixerPath::default(); CM119_MIXER_PATH_CAPACITY];
    let mut count = 0;
    let source = || Cm119MixerPathSource {
        name: c"Mic",
        element_index: 7,
        channel: MixerChannel::Right,
        direction: MixerDirection::Capture,
        volume_supported: false,
        switch_supported: false,
    };
    assert_eq!(
        append_cm119_mixer_path(&mut paths, &mut count, source()),
        Ok(())
    );
    assert_eq!(count, 0);
    let volume = || Cm119MixerPathSource {
        volume_supported: true,
        ..source()
    };
    count = CM119_MIXER_PATH_CAPACITY as u32;
    assert_eq!(
        append_cm119_mixer_path(&mut paths, &mut count, volume()),
        Err(AUDIO_UNSUPPORTED)
    );
    count = 0;
    let long_name = CString::new("M".repeat(CM119_MIXER_ELEMENT_NAME_CAPACITY)).unwrap();
    assert_eq!(
        append_cm119_mixer_path(
            &mut paths,
            &mut count,
            Cm119MixerPathSource {
                name: &long_name,
                ..volume()
            },
        ),
        Err(AUDIO_UNSUPPORTED)
    );
    assert_eq!(count, 0);
    assert!(paths[0].element.iter().all(|value| *value == 0));
}

#[cfg(unix)]
#[test]
fn cm119_resolver_rejects_null_output_and_invalid_utf8_before_opening_alsa() {
    let _serial = lock_fake();
    reset_fake_functions();
    assert_eq!(
        cm119_mixer_paths_resolve(ptr::null(), ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    let root = create_test_sysfs_root("cm119-release-invalid");
    let mut paths = ffi_cm119_mixer_paths();
    paths.abi_version = 42;
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast(),
            &mut paths,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    assert_unpublished(&paths);
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().open_count, 0));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn cm119_resolver_closes_alsa_at_each_setup_failure() {
    let _serial = lock_fake();
    let root = create_test_sysfs_root("cm119-release-setup");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    for fault in 0..4 {
        reset_fake_functions();
        FAKE_ALSA.with(|state| {
            let mut state = state.borrow_mut();
            match fault {
                0 => state.open_result = -1,
                1 => state.attach_result = -1,
                2 => state.register_result = -1,
                _ => state.load_result = -1,
            }
        });
        let mut paths = ffi_cm119_mixer_paths();
        paths.abi_version = 42;
        assert_eq!(
            cm119_mixer_paths_resolve_with_functions_and_root(
                &TEST_FUNCTIONS,
                &root,
                c"3-1:1.0".as_ptr(),
                &mut paths,
            ),
            AUDIO_ALSA_ERROR
        );
        assert_unpublished(&paths);
        FAKE_ALSA.with(|state| {
            let state = state.borrow();
            assert_eq!(state.open_count, 1);
            assert_eq!(state.close_count, u32::from(fault != 0));
            assert_eq!(state.attach_count, u32::from(fault > 0));
            assert_eq!(state.register_count, u32::from(fault > 1));
            assert_eq!(state.load_count, u32::from(fault > 2));
        });
    }
    fs::remove_dir_all(root).unwrap();
}

unsafe extern "C" fn missing_mixer_name(_element: *mut SndMixerElem) -> *const c_char {
    ptr::null()
}

static MISSING_NAME_FUNCTIONS: ffi::FunctionTable = ffi::FunctionTable {
    alsa: ffi::AlsaFunctions {
        selem_get_name: missing_mixer_name,
        ..TEST_FUNCTIONS.alsa
    },
    ..TEST_FUNCTIONS
};

#[cfg(unix)]
#[test]
fn cm119_resolver_skips_inactive_elements_and_closes_on_missing_names() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("cm119-release-inventory");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let mut inactive = mixer_element("Inactive", true, true);
    inactive.active = false;
    FAKE_ALSA.with(|state| {
        state.borrow_mut().inventory = vec![
            inactive,
            mixer_element("Mic", true, false),
            mixer_element("Speaker", false, true),
        ];
    });
    let mut paths = ffi_cm119_mixer_paths();
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_OK
    );
    assert_eq!(paths.rx_capture_path_count, 1);
    assert_eq!(paths.tx_playback_path_count, 1);
    assert_eq!(paths.sidetone_path_count, 0);
    assert_eq!(cm119_path_element(&paths.rx_capture_paths[0]), "Mic");
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &MISSING_NAME_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_ALSA_ERROR
    );
    assert_unpublished(&paths);
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 2));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn cm119_resolver_rejects_missing_capture_and_oversized_path_inventory_atomically() {
    let _serial = lock_fake();
    let root = create_test_sysfs_root("cm119-release-capacity");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let mic = mixer_element("Mic", true, false);
    let speaker = mixer_element("Speaker", false, true);
    let mut sidetone = mixer_element("Mic Playback", true, true);
    sidetone.capture_channels = [false, false];
    let mut boost = mixer_element("Auto Gain Control", false, false);
    boost.playback_switch = true;
    boost.playback_channels = [true, false];
    let long_name = "M".repeat(CM119_MIXER_ELEMENT_NAME_CAPACITY);
    let cases = vec![
        vec![speaker.clone()],
        vec![mic.clone(), mic.clone(), mic.clone(), speaker.clone()],
        vec![mic.clone(), sidetone.clone(), sidetone.clone(), sidetone],
        vec![
            mic.clone(),
            speaker.clone(),
            boost.clone(),
            boost.clone(),
            boost,
        ],
        vec![mixer_element(&long_name, true, false), speaker.clone()],
        vec![mic.clone(), mixer_element(&long_name, false, true)],
    ];
    for inventory in cases {
        reset_fake_functions();
        FAKE_ALSA.with(|state| state.borrow_mut().inventory = inventory);
        let mut paths = ffi_cm119_mixer_paths();
        paths.abi_version = 42;
        paths.rx_capture_path_count = 1;
        assert_eq!(
            cm119_mixer_paths_resolve_with_functions_and_root(
                &TEST_FUNCTIONS,
                &root,
                c"3-1:1.0".as_ptr(),
                &mut paths,
            ),
            AUDIO_UNSUPPORTED
        );
        assert_unpublished(&paths);
        FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));
    }
    fs::remove_dir_all(root).unwrap();
}
