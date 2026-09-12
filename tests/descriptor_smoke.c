/**
 * @file descriptor_smoke.c
 * @brief Verify that a C consumer can load the adapter's public descriptor.
 */

#include <assert.h>
#include <string.h>

#include "rptadv_portaudio_alsa_adapter/rptadv_portaudio_alsa_adapter.h"

static int32_t noop_tick(void *context, const float *input, float *output,
			 uint32_t frame_count)
{
	(void)context;
	(void)input;
	(void)output;
	(void)frame_count;
	return 0;
}

int main(void)
{
	const struct rptadv_audio_adapter_descriptor *descriptor =
		rptadv_portaudio_alsa_adapter_descriptor();
	struct rptadv_audio_stream_config stream_config = {
		.struct_size = sizeof(stream_config),
		.abi_version = RPTADV_AUDIO_ADAPTER_ABI_VERSION,
		.native_sample_rate_hz = 48000,
		.maximum_frame_count = 960,
		.input_device_index = RPTADV_AUDIO_DEFAULT_DEVICE,
		.output_device_index = RPTADV_AUDIO_DEFAULT_DEVICE,
		.input_device_channels = 1,
		.output_device_channels = 1,
		.native_tick = noop_tick,
		.native_tick_context = NULL,
	};
	struct rptadv_audio_stream_stats stats = {
		.struct_size = sizeof(stats),
	};
	struct rptadv_audio_mixer_config mixer_config = {
		.struct_size = sizeof(mixer_config),
		.card = "default",
		.element = "Capture",
		.element_index = 0,
		.channel = 0,
		.direction = RPTADV_AUDIO_MIXER_CAPTURE,
	};

	assert(descriptor != NULL);
	assert(descriptor->abi_version == RPTADV_AUDIO_ADAPTER_ABI_VERSION);
	assert(descriptor->struct_size == sizeof(*descriptor));
	assert(strcmp(descriptor->capability_name, "rptadv.portaudio-alsa-audio") == 0);
	assert(descriptor->stream_create != NULL);
	assert(descriptor->stream_start != NULL);
	assert(descriptor->stream_stop != NULL);
	assert(descriptor->stream_get_stats != NULL);
	assert(descriptor->stream_destroy != NULL);
	assert(descriptor->mixer_create != NULL);
	assert(descriptor->mixer_get_range_centibels != NULL);
	assert(descriptor->mixer_get_centibels != NULL);
	assert(descriptor->mixer_set_centibels != NULL);
	assert(descriptor->mixer_destroy != NULL);
	assert(stream_config.native_tick_context == NULL);
	assert(mixer_config.direction == RPTADV_AUDIO_MIXER_CAPTURE);
	assert(stats.abi_version == 0);
	assert(stats.callback_count == 0);
	assert(stats.callback_frame_count == 0);
	assert(stats.oversized_callback_count == 0);
	assert(stats.native_tick_failure_count == 0);
	assert(stats.input_overflow_count == 0);
	assert(stats.output_underflow_count == 0);
	assert(stats.input_clip_sample_count == 0);
	assert(stats.output_clip_sample_count == 0);
	assert(stats.input_peak == 0.0F);
	assert(stats.input_rms == 0.0F);
	assert(stats.output_peak == 0.0F);
	assert(stats.output_rms == 0.0F);
	assert(stats.last_portaudio_error == 0);
	return 0;
}
