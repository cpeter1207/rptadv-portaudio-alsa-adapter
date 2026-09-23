/**
 * @file rptadv_portaudio_alsa_adapter.h
 * @brief Stable C ABI for the rpt_advanced PortAudio/ALSA audio adapter.
 *
 * The adapter exposes canonical interleaved, normalized IEEE-754 binary32
 * stereo PCM to separate input-paced receive and DAC-paced transmit workers.
 * PortAudio performs conversion between that format and the physical device
 * format below the callbacks.
 */

#ifndef RPTADV_PORTAUDIO_ALSA_ADAPTER_H
#define RPTADV_PORTAUDIO_ALSA_ADAPTER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/** @brief ABI implemented by this adapter descriptor. */
#define RPTADV_AUDIO_ADAPTER_ABI_VERSION 2U

/** @brief Number of interleaved canonical PCM channels supplied to a worker. */
#define RPTADV_AUDIO_CANONICAL_CHANNELS 2U

/** @brief Fixed native sample rate accepted by ABI 2. */
#define RPTADV_AUDIO_NATIVE_SAMPLE_RATE_HZ 48000U

/** @brief Largest optional input or output buffer increase in milliseconds. */
#define RPTADV_AUDIO_MAX_EXTRA_BUFFER_MILLISECONDS 500U

/** @brief Select PortAudio's default input or output device. */
#define RPTADV_AUDIO_DEFAULT_DEVICE (-1)

/** @brief Scheduling policy or priority could not be queried at stream start. */
#define RPTADV_AUDIO_SCHEDULING_UNKNOWN (-1)

/** @brief Lowest portable normalized ALSA mixer setting. */
#define RPTADV_AUDIO_MIXER_NORMALIZED_MINIMUM 0U

/** @brief Highest portable normalized ALSA mixer setting. */
#define RPTADV_AUDIO_MIXER_NORMALIZED_MAXIMUM 999U

/** @brief Capacity of a returned stable USB interface-path string, including NUL. */
#define RPTADV_AUDIO_USB_INTERFACE_PATH_CAPACITY 256U

/** @brief Capacity of a returned USB serial-number string, including NUL. */
#define RPTADV_AUDIO_USB_SERIAL_CAPACITY 256U

/** @brief Maximum CM119 mixer paths in each semantic path class. */
#define RPTADV_AUDIO_CM119_MIXER_PATH_CAPACITY 2U

/** @brief Capacity of one returned ALSA simple-mixer element name, including NUL. */
#define RPTADV_AUDIO_CM119_MIXER_ELEMENT_NAME_CAPACITY 64U

/** @brief Opaque logical radio stream owning capture and playback endpoints. */
struct rptadv_audio_stream;

/** @brief Opaque ALSA simple-mixer control owned by the adapter. */
struct rptadv_audio_mixer;

/** @brief Result returned by an adapter operation. */
enum rptadv_audio_result {
	/** Operation completed. */
	RPTADV_AUDIO_OK = 0,
	/** A required pointer, structure size, or value was invalid. */
	RPTADV_AUDIO_INVALID_ARGUMENT = -1,
	/** Memory could not be reserved during control-plane setup. */
	RPTADV_AUDIO_NO_MEMORY = -2,
	/** PortAudio could not initialize, open, or control the stream. */
	RPTADV_AUDIO_PORTAUDIO_ERROR = -3,
	/** ALSA could not open or change the requested mixer control. */
	RPTADV_AUDIO_ALSA_ERROR = -4,
	/** The requested device or mixer element is not usable. */
	RPTADV_AUDIO_UNSUPPORTED = -5,
	/** A stream already owns one resolved physical PortAudio device. */
	RPTADV_AUDIO_DEVICE_BUSY = -6,
};

/**
 * @brief Input-paced receive worker implemented by the radio core.
 *
 * @param context Caller-owned callback context.
 * @param input Canonical interleaved stereo input containing @p frame_count frames.
 * @param frame_count Number of native PCM time frames in this invocation.
 * @return Zero after consuming the complete input block; nonzero aborts capture.
 *
 * The callback runs on PortAudio's real-time thread. It must not allocate,
 * lock, block, log, or perform I/O.
 */
typedef int32_t (*rptadv_audio_receive_worker)(void *context, const float *input,
					       uint32_t frame_count);

/**
 * @brief DAC-paced transmit worker implemented by the radio core.
 *
 * @param context Caller-owned callback context.
 * @param output Canonical interleaved stereo output to populate completely.
 * @param frame_count Number of native PCM time frames in this invocation.
 * @return Zero after producing the complete output block; nonzero aborts playback.
 *
 * The callback runs on PortAudio's real-time thread. It must not allocate,
 * lock, block, log, or perform I/O.
 */
typedef int32_t (*rptadv_audio_transmit_worker)(void *context, float *output,
					uint32_t frame_count);

/**
 * @brief Stream setup selected by the control plane before the device opens.
 *
 * The caller resolves a stable device identity before it supplies the resulting
 * PortAudio indexes. The adapter reserves each resolved physical input and
 * output device for the stream lifetime, so a conflicting open returns
 * @ref RPTADV_AUDIO_DEVICE_BUSY. Input and output device indexes use
 * @ref RPTADV_AUDIO_DEFAULT_DEVICE for the corresponding PortAudio default.
 * Mono physical input is duplicated on input and a mono output device receives
 * the average of canonical left and right output. Both workers always exchange
 * two interleaved canonical channels.
 *
 * The adapter requests PortAudio's default-low input and output latencies and
 * requests each direction's configured maximum frames per buffer. A nonzero
 * @p extra_output_buffer_milliseconds or @p extra_input_buffer_milliseconds
 * increases only the matching latency hint, by that amount beyond the
 * default-low/device-period baseline. Zero retains the default-low request.
 * These values are host hints, not latency guarantees.
 * A host block larger than its maximum is split into consecutive worker calls
 * containing at least one frame and no more than that direction's declared
 * maximum.
 * Receive and transmit workers may run concurrently; their contexts must be
 * disjoint or safe for concurrent access.
 */
struct rptadv_audio_stream_config {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Fixed native sample rate; use @ref RPTADV_AUDIO_NATIVE_SAMPLE_RATE_HZ. */
	uint32_t native_sample_rate_hz;
	/** Largest receive-worker block that the core has preallocated for. */
	uint32_t maximum_receive_frame_count;
	/** Largest transmit-worker block that the core has preallocated for. */
	uint32_t maximum_transmit_frame_count;
	/** PortAudio input-device index or @ref RPTADV_AUDIO_DEFAULT_DEVICE. */
	int32_t input_device_index;
	/** PortAudio output-device index or @ref RPTADV_AUDIO_DEFAULT_DEVICE. */
	int32_t output_device_index;
	/** Physical input-channel count: one or two. */
	uint32_t input_device_channels;
	/** Physical output-channel count: one or two. */
	uint32_t output_device_channels;
	/** Real-time input-paced receive worker. */
	rptadv_audio_receive_worker receive_worker;
	/** Opaque context returned unchanged to @ref receive_worker. */
	void *receive_worker_context;
	/** Real-time DAC-paced transmit worker. */
	rptadv_audio_transmit_worker transmit_worker;
	/** Opaque context returned unchanged to @ref transmit_worker. */
	void *transmit_worker_context;
	/** Optional extra PortAudio output-buffer request, in milliseconds (0-500). */
	uint32_t extra_output_buffer_milliseconds;
	/** Optional extra PortAudio input-buffer request, in milliseconds (0-500). */
	uint32_t extra_input_buffer_milliseconds;
};

/** @brief Lock-free, best-effort raw audio and callback snapshot. */
struct rptadv_audio_stream_stats {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this snapshot. */
	uint32_t abi_version;
	/** Number of PortAudio playback callbacks processed. */
	uint64_t callback_count;
	/** Number of physical playback frames processed. */
	uint64_t callback_frame_count;
	/** Capture or playback host blocks split to honor their declared maximum. */
	uint64_t oversized_callback_count;
	/** Number of failed receive- or transmit-worker invocations. */
	uint64_t worker_failure_count;
	/** Number of PortAudio input-overflow status notifications. */
	uint64_t input_overflow_count;
	/** Number of PortAudio output-underflow status notifications. */
	uint64_t output_underflow_count;
	/** Number of PortAudio control-plane errors observed by this stream. */
	uint64_t device_error_count;
	/** Number of raw input samples at or beyond full scale. */
	uint64_t input_clip_sample_count;
	/** Number of output samples at or beyond full scale. */
	uint64_t output_clip_sample_count;
	/** Peak absolute raw device-input sample since stream creation. */
	float input_peak;
	/** RMS raw device-input sample since stream creation. */
	float input_rms;
	/** Peak absolute device-output sample since stream creation. */
	float output_peak;
	/** RMS device-output sample since stream creation. */
	float output_rms;
	/** Last PortAudio error returned outside the callback, or zero. */
	int32_t last_portaudio_error;
	/** Most recent playback-callback duration, including preemption, in ns. */
	uint64_t callback_last_duration_ns;
	/** Maximum playback-callback duration since stream creation, in ns. */
	uint64_t callback_max_duration_ns;
	/** Positive excess over the prior playback block between callback starts. */
	uint64_t callback_last_start_delay_ns;
	/** Maximum start-gap excess in ns; not a kernel run-queue measurement. */
	uint64_t callback_max_start_delay_ns;
	/** Start gaps exceeding the tolerance; first/restarted callback excluded. */
	uint64_t callback_late_start_count;
	/** Late-start counting tolerance in ns (currently one millisecond). */
	uint64_t callback_late_start_tolerance_ns;
	/** Latest input-overflow notification, CLOCK_MONOTONIC ns; zero if none. */
	uint64_t last_input_xrun_monotonic_ns;
	/** Latest output-underflow notification, CLOCK_MONOTONIC ns; zero if none. */
	uint64_t last_output_xrun_monotonic_ns;
	/** Failed monotonic-clock reads; timing is unavailable for those callbacks. */
	uint64_t callback_clock_error_count;
	/** Number of PortAudio capture callbacks processed. */
	uint64_t capture_callback_count;
	/** Linux scheduling policy inherited by the capture callback, or -1 if unknown. */
	int32_t capture_scheduling_policy;
	/** Linux scheduling priority inherited by capture, or -1 if unknown. */
	int32_t capture_scheduling_priority;
	/** Nonzero when capture could not inherit preferred FIFO priority 99. */
	uint32_t capture_scheduling_limited;
	/** Linux scheduling policy inherited by the playback callback, or -1 if unknown. */
	int32_t playback_scheduling_policy;
	/** Linux scheduling priority inherited by playback, or -1 if unknown. */
	int32_t playback_scheduling_priority;
	/** Nonzero when playback could not inherit preferred FIFO priority 99. */
	uint32_t playback_scheduling_limited;
};

/**
 * @brief Actual immutable timing reported by PortAudio for an open stream.
 *
 * PortAudio may report a hardware sample rate or input/output latency that
 * differs from the values requested during stream creation.  These are the
 * best estimates available from the selected host API and are suitable for
 * control-plane playout-delay accounting, not per-sample synchronization.
 * The adapter copies the values into this ABI structure and does not expose
 * PortAudio types to a consumer.
 */
struct rptadv_audio_stream_timing {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this snapshot. */
	uint32_t abi_version;
	/** Actual input latency estimate in seconds. */
	double input_latency_seconds;
	/** Actual output latency estimate in seconds. */
	double output_latency_seconds;
	/** Actual native sample rate estimate in Hertz. */
	double sample_rate_hz;
};

/** @brief Direction of one hardware-mixer volume control. */
enum rptadv_audio_mixer_direction {
	/** CM119 ADC/capture mixer control. */
	RPTADV_AUDIO_MIXER_CAPTURE = 0,
	/** CM119 DAC/playback mixer control. */
	RPTADV_AUDIO_MIXER_PLAYBACK = 1
};

/** @brief Adapter-owned channel selection for a hardware mixer control. */
enum rptadv_audio_mixer_channel {
	/** First physical mixer channel. */
	RPTADV_AUDIO_MIXER_CHANNEL_LEFT = 0,
	/** Second physical mixer channel. */
	RPTADV_AUDIO_MIXER_CHANNEL_RIGHT = 1
};

/** @brief Capabilities exposed by one resolved CM119 mixer path. */
enum rptadv_audio_cm119_mixer_path_capability {
	/** The path has a native ALSA volume control. */
	RPTADV_AUDIO_CM119_MIXER_PATH_VOLUME = 1U << 0,
	/** The path has an ALSA capture or playback enable switch. */
	RPTADV_AUDIO_CM119_MIXER_PATH_SWITCH = 1U << 1,
};

/** @brief Control-plane description of one ALSA simple-mixer volume element. */
struct rptadv_audio_mixer_config {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** ALSA card or control-device name, for example `hw:CARD=Device`. */
	const char *card;
	/** ALSA simple-mixer element name, for example `Mic`. */
	const char *element;
	/** ALSA simple-mixer element index. */
	uint32_t element_index;
	/** One value from @ref rptadv_audio_mixer_channel. */
	uint32_t channel;
	/** One value from @ref rptadv_audio_mixer_direction. */
	uint32_t direction;
};

/**
 * @brief Control-plane description of an ALSA mixer selected by USB interface.
 *
 * The interface path is the stable Linux USB interface component, such as
 * `3-1:1.0`.  The adapter resolves it through `/sys/class/sound` before it
 * opens the matching ALSA mixer.  This avoids relying on an unstable ALSA card
 * number or on a host-specific device-discovery helper.
 */
struct rptadv_audio_usb_mixer_config {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Stable USB interface path used to select the ALSA sound card. */
	const char *usb_interface_path;
	/** ALSA simple-mixer element name, for example `Mic`. */
	const char *element;
	/** ALSA simple-mixer element index. */
	uint32_t element_index;
	/** One value from @ref rptadv_audio_mixer_channel. */
	uint32_t channel;
	/** One value from @ref rptadv_audio_mixer_direction. */
	uint32_t direction;
};

/**
 * @brief Stable USB identity and required PortAudio channel counts.
 *
 * Supply at least one of @ref usb_interface_path or @ref usb_serial.  When
 * both are supplied, they must identify the same physical USB device.  The
 * resolver rejects no match and more than one match rather than guessing from
 * a volatile ALSA-card or PortAudio-device index.
 */
struct rptadv_audio_usb_device_identity {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Linux USB topology or interface component, for example `3-1` or `3-1:1.0`, or NULL. */
	const char *usb_interface_path;
	/** Exact USB device serial number, or NULL when topology is sufficient. */
	const char *usb_serial;
	/** Required physical PortAudio input-channel count: one or two. */
	uint32_t input_device_channels;
	/** Required physical PortAudio output-channel count: one or two. */
	uint32_t output_device_channels;
};

/**
 * @brief PortAudio and ALSA selection resolved from a stable USB identity.
 *
 * The caller supplies @ref struct_size.  The returned device indexes are only
 * valid until PortAudio is reinitialized or the device topology changes, so a
 * caller resolves them immediately before opening the selected stream.
 */
struct rptadv_audio_usb_device_selection {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this selection. */
	uint32_t abi_version;
	/** ALSA card index resolved from sysfs. */
	uint32_t alsa_card_index;
	/** Exact PortAudio input-device index. */
	int32_t input_device_index;
	/** Exact PortAudio output-device index. */
	int32_t output_device_index;
};

/** @brief Policy used to select a USB audio device. */
enum rptadv_audio_usb_selection_policy {
	/** Select the one device named by an identifier and/or serial number. */
	RPTADV_AUDIO_USB_SELECTION_EXACT = 0,
	/** Select the usable USB device with the lowest ALSA card index. */
	RPTADV_AUDIO_USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD = 1,
};

/**
 * @brief Input for selection compatible with a legacy USB-radio device string.
 *
 * With @ref RPTADV_AUDIO_USB_SELECTION_EXACT, provide @ref device_identifier,
 * @ref usb_serial, or both. An identifier is either a stable USB topology such
 * as `3-1` or `3-1:1.0`, or a native ALSA identifier `hw:<card>` or
 * `hw:<card>,<pcm>`. ALSA aliases such as `default` and `plughw:` are rejected.
 * When both identifier and serial are given, they must identify the same
 * device. With @ref RPTADV_AUDIO_USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
 * leave both strings NULL; the first usable physical USB card is selected.
 */
struct rptadv_audio_usb_device_selector {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** One value from @ref rptadv_audio_usb_selection_policy. */
	uint32_t selection_policy;
	/** Optional topology or native `hw:` device identifier. */
	const char *device_identifier;
	/** Optional exact USB serial number. */
	const char *usb_serial;
	/** Required physical PortAudio input-channel count: one or two. */
	uint32_t input_device_channels;
	/** Required physical PortAudio output-channel count: one or two. */
	uint32_t output_device_channels;
};

/**
 * @brief Complete result of selecting a USB audio device.
 *
 * The adapter writes the canonical stable USB interface path and serial when
 * the device exposes one. An unavailable serial is returned as an empty
 * string. The nested selection is immediately suitable for stream creation.
 * On a non-@ref RPTADV_AUDIO_OK result, every field other than the caller's
 * @ref struct_size is reset to zero so a caller cannot reuse a partial or
 * stale identity.
 */
struct rptadv_audio_usb_device_match {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this match. */
	uint32_t abi_version;
	/** Canonical stable USB interface path, including its terminating NUL. */
	char usb_interface_path[RPTADV_AUDIO_USB_INTERFACE_PATH_CAPACITY];
	/** USB serial number, or an empty string when the device has none. */
	char usb_serial[RPTADV_AUDIO_USB_SERIAL_CAPACITY];
	/** Exact ALSA-card and PortAudio-device selection. */
	struct rptadv_audio_usb_device_selection selection;
};

/**
 * @brief One concrete ALSA simple-mixer path discovered on a CM119 interface.
 *
 * The copied element name, index, channel, and direction can be placed in a
 * @ref rptadv_audio_usb_mixer_config and opened with
 * @ref rptadv_audio_adapter_descriptor::mixer_create_for_usb_interface.
 * @ref capabilities says whether that resulting handle also supports volume,
 * a path switch, or both.  The adapter supports only the left and right
 * CM119 mixer channels; a device whose required path uses another ALSA channel
 * is rejected rather than remapped.
 */
struct rptadv_audio_cm119_mixer_path {
	/** NUL-terminated ALSA simple-mixer element name. */
	char element[RPTADV_AUDIO_CM119_MIXER_ELEMENT_NAME_CAPACITY];
	/** ALSA simple-mixer element index. */
	uint32_t element_index;
	/** One value from @ref rptadv_audio_mixer_channel. */
	uint32_t channel;
	/** One value from @ref rptadv_audio_mixer_direction. */
	uint32_t direction;
	/** Bitwise OR of @ref rptadv_audio_cm119_mixer_path_capability values. */
	uint32_t capabilities;
};

/**
 * @brief CM119 ALSA paths classified with the legacy USB-radio semantics.
 *
 * `tx_playback_paths[0]` is TX A and `[1]`, when present, is TX B.  Any
 * additional playback paths are intentionally left unchanged, matching the
 * legacy adapter. Receive capture and sidetone paths may each contain one or
 * two physical channels.
 * `rx_compatibility_switch_paths` contains the named legacy `Auto Gain
 * Control` playback-switch path when the interface provides it.  A zero count
 * means that optional semantic path is unavailable.
 */
struct rptadv_audio_cm119_mixer_paths {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this result. */
	uint32_t abi_version;
	/** Number of usable entries in @ref rx_capture_paths. */
	uint32_t rx_capture_path_count;
	/** Number of usable entries in @ref tx_playback_paths. */
	uint32_t tx_playback_path_count;
	/** Number of usable entries in @ref sidetone_paths. */
	uint32_t sidetone_path_count;
	/** Number of usable entries in @ref rx_compatibility_switch_paths. */
	uint32_t rx_compatibility_switch_path_count;
	/** CM119 ADC capture paths. */
	struct rptadv_audio_cm119_mixer_path
		rx_capture_paths[RPTADV_AUDIO_CM119_MIXER_PATH_CAPACITY];
	/** CM119 DAC paths: TX A then TX B. */
	struct rptadv_audio_cm119_mixer_path
		tx_playback_paths[RPTADV_AUDIO_CM119_MIXER_PATH_CAPACITY];
	/** CM119 Mic playback/sidetone paths. */
	struct rptadv_audio_cm119_mixer_path
		sidetone_paths[RPTADV_AUDIO_CM119_MIXER_PATH_CAPACITY];
	/** Optional legacy receive compatibility-switch paths. */
	struct rptadv_audio_cm119_mixer_path
		rx_compatibility_switch_paths[RPTADV_AUDIO_CM119_MIXER_PATH_CAPACITY];
};

/**
 * @brief Versioned function table exported by the adapter shared object.
 *
 * All descriptor functions are control-plane operations. The caller must
 * serialize lifecycle and mixer calls for each handle, and must never call
 * them from @ref rptadv_audio_receive_worker or @ref rptadv_audio_transmit_worker.
 */
struct rptadv_audio_adapter_descriptor {
	/** Size of this descriptor. */
	uint32_t struct_size;
	/** ABI implemented by every function in this table. */
	uint32_t abi_version;
	/** Stable capability name. */
	const char *capability_name;
	/**
	 * @brief Open one exclusively owned full-duplex PortAudio stream.
	 *
	 * The adapter releases its control-plane device lease after this stream is
	 * destroyed or if opening fails after reservation.
	 */
	enum rptadv_audio_result (*stream_create)(
		const struct rptadv_audio_stream_config *config,
		struct rptadv_audio_stream **stream);
	/**
	 * @brief Start callbacks at the highest permitted Linux FIFO priority.
	 *
	 * The adapter first tries priority 99 and then lower FIFO priorities. If
	 * elevation or scheduling metadata is unavailable, callbacks inherit the
	 * caller's existing scheduling and startup continues. Stream statistics
	 * report the actual inherited policy/priority, or -1 when querying them was
	 * impossible, plus a nonfatal limitation flag. A caller scheduling change is
	 * restored after PortAudio starts both callbacks; restoration failure aborts
	 * a successfully started stream.
	 */
	enum rptadv_audio_result (*stream_start)(struct rptadv_audio_stream *stream);
	/** Stop callbacks for a running stream. */
	enum rptadv_audio_result (*stream_stop)(struct rptadv_audio_stream *stream);
	/** Obtain a lock-free best-effort snapshot without touching callback state. */
	enum rptadv_audio_result (*stream_get_stats)(
		const struct rptadv_audio_stream *stream,
		struct rptadv_audio_stream_stats *stats);
	/** Stop callbacks, close the PortAudio stream, and free its resources. */
	void (*stream_destroy)(struct rptadv_audio_stream *stream);
	/** Open one ALSA simple-mixer volume element. */
	enum rptadv_audio_result (*mixer_create)(
		const struct rptadv_audio_mixer_config *config,
		struct rptadv_audio_mixer **mixer);
	/** Read the element's available gain range in centibels. */
	enum rptadv_audio_result (*mixer_get_range_centibels)(
		const struct rptadv_audio_mixer *mixer, int64_t *minimum, int64_t *maximum);
	/** Read the selected channel's current gain in centibels. */
	enum rptadv_audio_result (*mixer_get_centibels)(
		const struct rptadv_audio_mixer *mixer, int64_t *value);
	/** Set the selected channel's gain in centibels. */
	enum rptadv_audio_result (*mixer_set_centibels)(
		struct rptadv_audio_mixer *mixer, int64_t value);
	/** Close an ALSA mixer element and free its resources. */
	void (*mixer_destroy)(struct rptadv_audio_mixer *mixer);
	/**
	 * @brief Open a mixer after resolving a stable USB interface path to ALSA.
	 *
	 * This is a Linux control-plane operation.  It is not callable from the
	 * native audio callback.
	 */
	enum rptadv_audio_result (*mixer_create_for_usb_interface)(
		const struct rptadv_audio_usb_mixer_config *config,
		struct rptadv_audio_mixer **mixer);
	/** Read the selected channel's native ALSA mixer-step range. */
	enum rptadv_audio_result (*mixer_get_range_steps)(
		const struct rptadv_audio_mixer *mixer, int64_t *minimum, int64_t *maximum);
	/** Read the selected channel's current native ALSA mixer-step value. */
	enum rptadv_audio_result (*mixer_get_steps)(
		const struct rptadv_audio_mixer *mixer, int64_t *value);
	/** Set the selected channel's native ALSA mixer-step value. */
	enum rptadv_audio_result (*mixer_set_steps)(
		struct rptadv_audio_mixer *mixer, int64_t value);
	/**
	 * @brief Read the selected channel on a portable 0 through 999 scale.
	 *
	 * The native minimum and maximum map exactly to zero and 999. Intermediate
	 * values use nearest-step rounding because ALSA controls are discrete.
	 */
	enum rptadv_audio_result (*mixer_get_normalized)(
		const struct rptadv_audio_mixer *mixer, uint32_t *value);
	/** Set the selected channel on a portable 0 through 999 scale. */
	enum rptadv_audio_result (*mixer_set_normalized)(
		struct rptadv_audio_mixer *mixer, uint32_t value);
	/** Read whether the selected ALSA capture or playback path is enabled. */
	enum rptadv_audio_result (*mixer_get_switch)(
		const struct rptadv_audio_mixer *mixer, uint32_t *enabled);
	/** Enable or disable the selected ALSA capture or playback path; use zero or one. */
	enum rptadv_audio_result (*mixer_set_switch)(
		struct rptadv_audio_mixer *mixer, uint32_t enabled);
	/**
	 * @brief Resolve one stable USB identity to exact ALSA and PortAudio devices.
	 *
	 * The Linux PortAudio ALSA backend must expose exactly one matching native
	 * `hw:<card>,<device>` entry per requested direction.  Other host APIs and
	 * ambiguous ALSA-plugin names are rejected rather than selected by index.
	 */
	enum rptadv_audio_result (*usb_device_resolve)(
		const struct rptadv_audio_usb_device_identity *identity,
		struct rptadv_audio_usb_device_selection *selection);
	/**
	 * @brief Select a USB audio device from a legacy-compatible identifier.
	 *
	 * This entry maps a configured topology, serial, legacy
	 * native `hw:` identifier, or automatic selection to stable identity and
	 * exact PortAudio indexes without exposing host inventory details to a
	 * channel adapter.
	 */
	enum rptadv_audio_result (*usb_device_select)(
		const struct rptadv_audio_usb_device_selector *selector,
		struct rptadv_audio_usb_device_match *match);
	/**
	 * @brief Read immutable PortAudio timing after a stream has opened.
	 *
	 * This is a control-plane query and must not run from an audio callback.
	 * On failure, every field other than the caller's @ref struct_size is reset
	 * to zero.  A successful result does not imply that the stream is active.
	 */
	enum rptadv_audio_result (*stream_get_timing)(
		const struct rptadv_audio_stream *stream,
		struct rptadv_audio_stream_timing *timing);
	/**
	 * @brief Discover CM119 mixer paths using the legacy USB-radio semantics.
	 *
	 * The selected stable USB interface is resolved to its ALSA card, then its
	 * active simple-mixer elements are classified exactly as the legacy driver:
	 * capture-volume paths are RX, playback-volume paths on the same element are
	 * sidetone, other playback-volume paths are TX A/B, and the named `Auto Gain
	 * Control` playback switch is an optional receive compatibility path.  This
	 * is a control-plane query and never changes hardware state.  The caller
	 * supplies @ref rptadv_audio_cm119_mixer_paths::struct_size.  On failure the
	 * adapter preserves that field and clears the rest of the output.
	 */
	enum rptadv_audio_result (*cm119_mixer_paths_resolve)(
		const char *usb_interface_path,
		struct rptadv_audio_cm119_mixer_paths *paths);
};

/**
 * @brief Return the static descriptor for this shared-object ABI.
 * @return Never-null pointer valid for the lifetime of the loaded shared object.
 */
const struct rptadv_audio_adapter_descriptor *
rptadv_portaudio_alsa_adapter_descriptor(void);

#ifdef __cplusplus
}
#endif

#endif
