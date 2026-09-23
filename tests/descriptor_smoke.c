/**
 * @file descriptor_smoke.c
 * @brief Verify that a C consumer can load the adapter's public descriptor.
 */

#include <assert.h>
#include <string.h>

#include "rptadv_portaudio_alsa_adapter/rptadv_portaudio_alsa_adapter.h"

static int32_t noop_receive(void *context, const float *input,
			    uint32_t frame_count)
{
	(void)context;
	(void)input;
	(void)frame_count;
	return 0;
}

static int32_t noop_transmit(void *context, float *output, uint32_t frame_count)
{
	(void)context;
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
		.native_sample_rate_hz = RPTADV_AUDIO_NATIVE_SAMPLE_RATE_HZ,
		.maximum_receive_frame_count = 960,
		.maximum_transmit_frame_count = 960,
		.input_device_index = RPTADV_AUDIO_DEFAULT_DEVICE,
		.output_device_index = RPTADV_AUDIO_DEFAULT_DEVICE,
		.input_device_channels = 1,
		.output_device_channels = 1,
		.receive_worker = noop_receive,
		.receive_worker_context = NULL,
		.transmit_worker = noop_transmit,
		.transmit_worker_context = NULL,
		.extra_output_buffer_milliseconds = 0,
		.extra_input_buffer_milliseconds = 0,
	};
	struct rptadv_audio_stream_stats stats = {
		.struct_size = sizeof(stats),
	};
	struct rptadv_audio_stream_timing timing = {
		.struct_size = sizeof(timing),
	};
	struct rptadv_audio_mixer_config mixer_config = {
		.struct_size = sizeof(mixer_config),
		.card = "default",
		.element = "Capture",
		.element_index = 0,
		.channel = 0,
		.direction = RPTADV_AUDIO_MIXER_CAPTURE,
	};
	struct rptadv_audio_usb_mixer_config usb_mixer_config = {
		.struct_size = sizeof(usb_mixer_config),
		.usb_interface_path = "3-1:1.0",
		.element = "Capture",
		.element_index = 0,
		.channel = RPTADV_AUDIO_MIXER_CHANNEL_LEFT,
		.direction = RPTADV_AUDIO_MIXER_CAPTURE,
	};
	struct rptadv_audio_usb_device_identity usb_identity = {
		.struct_size = sizeof(usb_identity),
		.usb_interface_path = "3-1:1.0",
		.usb_serial = "CM119-A",
		.input_device_channels = 1,
		.output_device_channels = 1,
	};
	struct rptadv_audio_usb_device_selection usb_selection = {
		.struct_size = sizeof(usb_selection),
	};
	struct rptadv_audio_usb_device_selector usb_selector = {
		.struct_size = sizeof(usb_selector),
		.selection_policy = RPTADV_AUDIO_USB_SELECTION_EXACT,
		.device_identifier = "hw:4,0",
		.usb_serial = "CM119-A",
		.input_device_channels = 1,
		.output_device_channels = 1,
	};
	struct rptadv_audio_usb_device_match usb_match = {
		.struct_size = sizeof(usb_match),
	};
	struct rptadv_audio_cm119_mixer_paths cm119_paths = {
		.struct_size = sizeof(cm119_paths),
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
	assert(descriptor->mixer_create_for_usb_interface != NULL);
	assert(descriptor->mixer_get_range_steps != NULL);
	assert(descriptor->mixer_get_steps != NULL);
	assert(descriptor->mixer_set_steps != NULL);
	assert(descriptor->mixer_get_normalized != NULL);
	assert(descriptor->mixer_set_normalized != NULL);
	assert(descriptor->mixer_get_switch != NULL);
	assert(descriptor->mixer_set_switch != NULL);
	assert(descriptor->usb_device_resolve != NULL);
	assert(descriptor->usb_device_select != NULL);
	assert(descriptor->stream_get_timing != NULL);
	assert(descriptor->cm119_mixer_paths_resolve != NULL);
	assert(stream_config.receive_worker_context == NULL);
	assert(stream_config.transmit_worker_context == NULL);
	assert(stream_config.extra_output_buffer_milliseconds == 0);
	assert(stream_config.extra_input_buffer_milliseconds == 0);
	assert(mixer_config.direction == RPTADV_AUDIO_MIXER_CAPTURE);
	assert(usb_mixer_config.direction == RPTADV_AUDIO_MIXER_CAPTURE);
	assert(usb_identity.input_device_channels == 1);
	assert(usb_selection.abi_version == 0);
	assert(usb_selector.selection_policy == RPTADV_AUDIO_USB_SELECTION_EXACT);
	assert(usb_match.abi_version == 0);
	assert(cm119_paths.abi_version == 0);
	assert(cm119_paths.rx_capture_path_count == 0);
	assert(cm119_paths.tx_playback_path_count == 0);
	assert(cm119_paths.sidetone_path_count == 0);
	assert(cm119_paths.rx_compatibility_switch_path_count == 0);
	assert(RPTADV_AUDIO_CM119_MIXER_PATH_CAPACITY == 2U);
	assert(RPTADV_AUDIO_CM119_MIXER_ELEMENT_NAME_CAPACITY >= 19U);
	assert(RPTADV_AUDIO_MIXER_NORMALIZED_MINIMUM == 0U);
	assert(RPTADV_AUDIO_MIXER_NORMALIZED_MAXIMUM == 999U);
	assert(RPTADV_AUDIO_SCHEDULING_UNKNOWN == -1);
	assert(stats.abi_version == 0);
	assert(stats.callback_count == 0);
	assert(stats.callback_frame_count == 0);
	assert(stats.oversized_callback_count == 0);
	assert(stats.worker_failure_count == 0);
	assert(stats.input_overflow_count == 0);
	assert(stats.output_underflow_count == 0);
	assert(stats.input_clip_sample_count == 0);
	assert(stats.output_clip_sample_count == 0);
	assert(stats.input_peak == 0.0F);
	assert(stats.input_rms == 0.0F);
	assert(stats.output_peak == 0.0F);
	assert(stats.output_rms == 0.0F);
	assert(stats.last_portaudio_error == 0);
	assert(stats.capture_scheduling_policy == 0);
	assert(stats.capture_scheduling_priority == 0);
	assert(stats.capture_scheduling_limited == 0U);
	assert(stats.playback_scheduling_policy == 0);
	assert(stats.playback_scheduling_priority == 0);
	assert(stats.playback_scheduling_limited == 0U);
	assert(timing.abi_version == 0);
	assert(timing.input_latency_seconds == 0.0);
	assert(timing.output_latency_seconds == 0.0);
	assert(timing.sample_rate_hz == 0.0);
	return 0;
}
