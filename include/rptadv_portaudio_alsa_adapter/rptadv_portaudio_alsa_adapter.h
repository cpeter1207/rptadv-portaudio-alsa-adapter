/**
 * @file rptadv_portaudio_alsa_adapter.h
 * @brief Stable C ABI for the rpt_advanced PortAudio/ALSA audio adapter.
 *
 * The adapter exposes canonical interleaved, normalized IEEE-754 binary32
 * stereo PCM to its native-tick callback. PortAudio performs conversion between
 * that format and the physical device format below the callback.
 */

#ifndef RPTADV_PORTAUDIO_ALSA_ADAPTER_H
#define RPTADV_PORTAUDIO_ALSA_ADAPTER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/** @brief ABI implemented by this adapter descriptor. */
#define RPTADV_AUDIO_ADAPTER_ABI_VERSION 1U

/** @brief Number of interleaved canonical PCM channels supplied to a tick. */
#define RPTADV_AUDIO_CANONICAL_CHANNELS 2U

/** @brief Select PortAudio's default input or output device. */
#define RPTADV_AUDIO_DEFAULT_DEVICE (-1)

/** @brief Opaque PortAudio stream owned by the adapter. */
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
};

/**
 * @brief Bounded native-tick callback implemented by the radio core.
 *
 * @param context Caller-owned callback context.
 * @param input Canonical interleaved stereo input containing @p frame_count frames.
 * @param output Canonical interleaved stereo output to populate with @p frame_count frames.
 * @param frame_count Number of native PCM time frames in this invocation.
 * @return Zero after producing the complete output block; nonzero aborts the stream.
 *
 * The callback runs on PortAudio's real-time thread. It must not allocate,
 * lock, block, log, or perform I/O.
 */
typedef int32_t (*rptadv_audio_native_tick)(void *context, const float *input,
					    float *output, uint32_t frame_count);

/**
 * @brief Stream setup selected by the control plane before the device opens.
 *
 * The caller resolves and exclusively owns a stable device identity before it
 * supplies the resulting PortAudio indexes. Input and output device indexes
 * use @ref RPTADV_AUDIO_DEFAULT_DEVICE for the corresponding PortAudio
 * default. Device channels are one or two; a mono device is duplicated on
 * input and receives the average of canonical left and right output. The
 * native callback always receives two interleaved channels.
 *
 * The adapter requests PortAudio's default-low input and output latencies and
 * requests @ref maximum_frame_count frames per buffer. A callback with more
 * than that maximum is split into consecutive native ticks, each containing
 * at least one and at most @ref maximum_frame_count frames.
 */
struct rptadv_audio_stream_config {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Fixed native sample rate for the lifetime of the stream. */
	uint32_t native_sample_rate_hz;
	/** Largest callback block that the core has preallocated for. */
	uint32_t maximum_frame_count;
	/** PortAudio input-device index or @ref RPTADV_AUDIO_DEFAULT_DEVICE. */
	int32_t input_device_index;
	/** PortAudio output-device index or @ref RPTADV_AUDIO_DEFAULT_DEVICE. */
	int32_t output_device_index;
	/** Physical input-channel count: one or two. */
	uint32_t input_device_channels;
	/** Physical output-channel count: one or two. */
	uint32_t output_device_channels;
	/** Real-time native-tick callback. */
	rptadv_audio_native_tick native_tick;
	/** Opaque context returned unchanged to @ref native_tick. */
	void *native_tick_context;
};

/** @brief Lock-free, best-effort raw audio and callback snapshot. */
struct rptadv_audio_stream_stats {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this snapshot. */
	uint32_t abi_version;
	/** Number of PortAudio callbacks processed. */
	uint64_t callback_count;
	/** Number of physical callback frames processed. */
	uint64_t callback_frame_count;
	/** Number of host blocks split to honor the configured maximum frame count. */
	uint64_t oversized_callback_count;
	/** Number of failed native-tick invocations. */
	uint64_t native_tick_failure_count;
	/** Number of PortAudio input-overflow status notifications. */
	uint64_t input_overflow_count;
	/** Number of PortAudio output-underflow status notifications. */
	uint64_t output_underflow_count;
	/** Number of PortAudio control-plane errors observed by this stream. */
	uint64_t device_error_count;
	/** Adapter capture-queue capacity in frames; zero for this direct path. */
	uint64_t input_queue_capacity_frames;
	/** Adapter capture-queue occupancy in frames; zero for this direct path. */
	uint64_t input_queue_occupancy_frames;
	/** Adapter playback-queue capacity in frames; zero for this direct path. */
	uint64_t output_queue_capacity_frames;
	/** Adapter playback-queue occupancy in frames; zero for this direct path. */
	uint64_t output_queue_occupancy_frames;
	/** Adapter playback frames dropped; zero for this direct callback path. */
	uint64_t output_queue_dropped_frame_count;
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
 * @brief Versioned function table exported by the adapter shared object.
 *
 * All descriptor functions are control-plane operations. The caller must
 * serialize lifecycle and mixer calls for each handle, and must never call
 * them from @ref rptadv_audio_native_tick.
 */
struct rptadv_audio_adapter_descriptor {
	/** Size of this descriptor. */
	uint32_t struct_size;
	/** ABI implemented by every function in this table. */
	uint32_t abi_version;
	/** Stable capability name. */
	const char *capability_name;
	/** Open one full-duplex PortAudio stream. */
	enum rptadv_audio_result (*stream_create)(
		const struct rptadv_audio_stream_config *config,
		struct rptadv_audio_stream **stream);
	/** Start callbacks for an opened stream. */
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
