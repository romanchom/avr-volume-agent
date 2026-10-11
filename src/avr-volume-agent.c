#define _POSIX_C_SOURCE 200809L

#include <math.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include <pipewire/filter.h>
#include <pipewire/pipewire.h>
#include "denon_protocol.h"
#include <spa/param/audio/format.h>
#include <spa/param/audio/raw.h>
#include <spa/param/props.h>
#include <spa/pod/builder.h>
#include <spa/pod/iter.h>

#define CHANNELS 6
#define RATE 48000

struct app;

struct port {
	struct app *app;
};

struct app {
	struct pw_main_loop *loop;
	struct pw_filter *filter;
	struct port *input[CHANNELS];
	struct port *output[CHANNELS];
	float volume;
	float restore_volume;
	bool mute;
	bool restore_mute;
	bool restore_captured;
	char target_sink[256];
	char node_name[128];
	char avr_host[256];
	uint16_t avr_port;
	double max_code;
	uint64_t control_after_ms;
	uint64_t restore_guard_until_ms;
	pthread_t network_thread;
	pthread_mutex_t network_lock;
	bool network_enabled;
	bool write_enabled;
	bool network_started;
	bool initial_status_logged;
	bool stop_network;
	bool network_ready;
	bool desired_mute;
	bool status_synced;
	int desired_code;
};

static const char *const channel_names[CHANNELS] = {
	"FL", "FR", "RL", "RR", "FC", "LFE"
};

static const uint32_t channel_ids[CHANNELS] = {
	SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR, SPA_AUDIO_CHANNEL_RL,
	SPA_AUDIO_CHANNEL_RR, SPA_AUDIO_CHANNEL_FC, SPA_AUDIO_CHANNEL_LFE
};

static uint64_t monotonic_milliseconds(void)
{
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}

static const struct spa_pod *build_props(struct spa_pod_builder *builder,
		float volume, bool mute);

static const struct spa_pod *build_audio_format(struct spa_pod_builder *builder,
		uint32_t channels, const uint32_t *positions)
{
	return spa_pod_builder_add_object(builder,
			SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
			SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_audio),
			SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
			SPA_FORMAT_AUDIO_format, SPA_POD_Id(SPA_AUDIO_FORMAT_F32),
			SPA_FORMAT_AUDIO_rate, SPA_POD_Int(RATE),
			SPA_FORMAT_AUDIO_channels, SPA_POD_Int(channels),
			SPA_FORMAT_AUDIO_position, SPA_POD_Array(sizeof(uint32_t),
				SPA_TYPE_Id, channels, positions));
}

static const struct spa_pod *build_props(struct spa_pod_builder *builder,
		float volume, bool mute)
{
	float channel_volumes[CHANNELS];
	float soft_volumes[CHANNELS];
	uint32_t i;

	for (i = 0; i < CHANNELS; i++) {
		channel_volumes[i] = volume;
		soft_volumes[i] = 1.0f;
	}

	return spa_pod_builder_add_object(builder,
			SPA_TYPE_OBJECT_Props, SPA_PARAM_Props,
			SPA_ID_INVALID, SPA_PROP_channelVolumes, SPA_POD_PROP_FLAG_HARDWARE,
				SPA_POD_Array(sizeof(float), SPA_TYPE_Float, CHANNELS,
					channel_volumes),
			SPA_PROP_channelMap, SPA_POD_Array(sizeof(uint32_t), SPA_TYPE_Id,
				CHANNELS, channel_ids),
			SPA_ID_INVALID, SPA_PROP_mute, SPA_POD_PROP_FLAG_HARDWARE,
				SPA_POD_Bool(mute),
			SPA_PROP_softMute, SPA_POD_Bool(false),
			SPA_PROP_softVolumes, SPA_POD_Array(sizeof(float), SPA_TYPE_Float,
				CHANNELS, soft_volumes));
}

static const struct spa_pod *build_volume_info(struct spa_pod_builder *builder)
{
	return spa_pod_builder_add_object(builder,
			SPA_TYPE_OBJECT_PropInfo, SPA_PARAM_PropInfo,
			SPA_PROP_INFO_id, SPA_POD_Id(SPA_PROP_channelVolumes),
			SPA_PROP_INFO_description, SPA_POD_String("AVR volume"),
			SPA_PROP_INFO_type, SPA_POD_CHOICE_RANGE_Float(1.0f, 0.0f, 1.0f),
			SPA_PROP_INFO_container, SPA_POD_Id(SPA_TYPE_Array));
}

static const struct spa_pod *build_mute_info(struct spa_pod_builder *builder)
{
	return spa_pod_builder_add_object(builder,
			SPA_TYPE_OBJECT_PropInfo, SPA_PARAM_PropInfo,
			SPA_PROP_INFO_id, SPA_POD_Id(SPA_PROP_mute),
			SPA_PROP_INFO_description, SPA_POD_String("AVR mute"),
			SPA_PROP_INFO_type, SPA_POD_CHOICE_Bool(false));
}

static void on_process(void *userdata, struct spa_io_position *position)
{
	struct app *app = userdata;
	const uint32_t frames = position == NULL ? 0 : position->clock.duration;
	uint32_t i;
	if (frames == 0)
		return;

	for (i = 0; i < CHANNELS; i++) {
		float *input = pw_filter_get_dsp_buffer(app->input[i], frames);
		float *output = pw_filter_get_dsp_buffer(app->output[i], frames);
		if (output == NULL)
			continue;
		if (input == NULL)
			memset(output, 0, frames * sizeof(float));
		else
			memcpy(output, input, frames * sizeof(float));
	}
}

static void on_param_changed(void *userdata, void *port_data, uint32_t id,
		const struct spa_pod *param)
{
	struct app *app = userdata;
	const struct spa_pod_prop *prop;
	float values[CHANNELS];
	float volume = app->volume;
	bool mute = app->mute;
	bool changed = false;
	int n_values;

	if (port_data != NULL || id != SPA_PARAM_Props || param == NULL ||
			!spa_pod_is_object_type(param, SPA_TYPE_OBJECT_Props))
		return;

	prop = spa_pod_find_prop(param, NULL, SPA_PROP_channelVolumes);
	if (prop != NULL) {
		n_values = spa_pod_copy_array(&prop->value, SPA_TYPE_Float,
				values, CHANNELS);
		if (n_values > 0)
			volume = values[0];
	} else {
		prop = spa_pod_find_prop(param, NULL, SPA_PROP_volume);
		if (prop != NULL)
			spa_pod_get_float(&prop->value, &volume);
	}
	prop = spa_pod_find_prop(param, NULL, SPA_PROP_mute);
	if (prop != NULL)
		spa_pod_get_bool(&prop->value, &mute);
	if (!app->status_synced && !app->restore_captured) {
		app->restore_volume = volume;
		app->restore_mute = mute;
		app->restore_captured = true;
	}
	changed = fabsf(volume - app->volume) > 0.00001f || mute != app->mute;
	if (!changed)
		return;
	if (app->status_synced && monotonic_milliseconds() <
			app->restore_guard_until_ms &&
			fabsf(volume - app->restore_volume) < 0.01f) {
		if (mute == app->mute)
			return;
		volume = app->volume;
	}
	if (app->network_enabled && app->status_synced &&
			monotonic_milliseconds() < app->control_after_ms) {
		uint8_t pod_buffer[1024];
		struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(pod_buffer,
				sizeof(pod_buffer));
		const struct spa_pod *current = build_props(&builder, app->volume,
				app->mute);
		if (current != NULL)
			pw_filter_update_params(app->filter, NULL, &current, 1);
		return;
	}

	if (volume < 0.0f)
		volume = 0.0f;
	if (volume > 1.0f)
		volume = 1.0f;
	app->volume = volume;
	app->mute = mute;
	if (!app->network_enabled) {
		uint8_t pod_buffer[1024];
		struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(pod_buffer,
				sizeof(pod_buffer));
		const struct spa_pod *updated = build_props(&builder, volume, mute);
		if (updated != NULL)
			pw_filter_update_params(app->filter, NULL, &updated, 1);
	} else {
		pthread_mutex_lock(&app->network_lock);
		if (app->network_ready) {
			app->desired_code = denon_slider_to_code(volume, app->max_code);
			app->desired_mute = mute;
		}
		pthread_mutex_unlock(&app->network_lock);
	}
}

static int on_avr_status(struct spa_loop *loop, bool async, uint32_t seq,
		const void *data, size_t size, void *userdata)
{
	struct app *app = userdata;
	const struct denon_status *status = data;
	uint8_t buffer[1024];
	struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
	const struct spa_pod *props;
	(void)loop;
	(void)async;
	(void)seq;

	if (size != sizeof(*status) || status->max_code == 0)
		return -EINVAL;
	if (!app->status_synced && !app->restore_captured) {
		app->restore_volume = app->volume;
		app->restore_mute = app->mute;
		app->restore_captured = true;
	}
	if (!app->initial_status_logged) {
		fprintf(stderr, "connected to AVR: volume %.1f/%.1f, mute %s\n",
				status->volume_code, status->max_code,
				status->mute ? "on" : "off");
		app->initial_status_logged = true;
	}
	app->volume = denon_code_to_channel_volume(status->volume_code,
			status->max_code);
	app->mute = status->mute;
	props = build_props(&builder, app->volume, app->mute);
	if (props != NULL)
		pw_filter_update_params(app->filter, NULL, &props, 1);

	pthread_mutex_lock(&app->network_lock);
	app->max_code = status->max_code;
	app->desired_code = (int)lround(status->volume_code);
	app->desired_mute = status->mute;
	app->network_ready = true;
	pthread_mutex_unlock(&app->network_lock);
	if (!app->status_synced) {
		app->status_synced = true;
		app->control_after_ms = monotonic_milliseconds() + 1000;
		app->restore_guard_until_ms = monotonic_milliseconds() + 2000;
	}
	return 0;
}

static void dispatch_avr_status(struct app *app,
		const struct denon_status *status)
{
	pw_loop_invoke(pw_main_loop_get_loop(app->loop), on_avr_status, 0,
			status, sizeof(*status), false, app);
}

static int connect_avr(const char *host, uint16_t port)
{
	struct addrinfo hints = { .ai_family = AF_UNSPEC, .ai_socktype = SOCK_STREAM };
	struct addrinfo *addresses = NULL, *address;
	char service[8];
	int fd = -1;
	int error;
	snprintf(service, sizeof(service), "%u", port);
	if (getaddrinfo(host, service, &hints, &addresses) != 0)
		return -1;
	for (address = addresses; address != NULL; address = address->ai_next) {
		struct pollfd pfd;
		int flags, socket_error = 0;
		socklen_t error_size = sizeof(socket_error);
		fd = socket(address->ai_family, address->ai_socktype,
				address->ai_protocol);
		if (fd < 0)
			continue;
		flags = fcntl(fd, F_GETFL, 0);
		fcntl(fd, F_SETFL, flags | O_NONBLOCK);
		error = connect(fd, address->ai_addr, address->ai_addrlen);
		if (error < 0 && errno != EINPROGRESS) {
			close(fd);
			fd = -1;
			continue;
		}
		pfd = (struct pollfd){ .fd = fd, .events = POLLOUT };
		if (poll(&pfd, 1, 1500) <= 0 ||
				getsockopt(fd, SOL_SOCKET, SO_ERROR, &socket_error,
					&error_size) < 0 || socket_error != 0) {
			close(fd);
			fd = -1;
			continue;
		}
		fcntl(fd, F_SETFL, flags);
		break;
	}
	freeaddrinfo(addresses);
	return fd;
}

static int send_command(int fd, const char *command)
{
	size_t length = strlen(command), sent = 0;
	while (sent < length) {
		ssize_t count = send(fd, command + sent, length - sent, MSG_NOSIGNAL);
		if (count <= 0)
			return -1;
		sent += (size_t)count;
	}
	return 0;
}

static bool network_should_stop(struct app *app)
{
	bool stop;
	pthread_mutex_lock(&app->network_lock);
	stop = app->stop_network;
	pthread_mutex_unlock(&app->network_lock);
	return stop;
}

static void *network_worker(void *userdata)
{
	struct app *app = userdata;
	while (!network_should_stop(app)) {
		struct denon_status status = { .max_code = 98, .mute = false };
		bool ready = false;
		int fd = connect_avr(app->avr_host, app->avr_port);
		int last_code = -1, last_mute = -1;
		char buffer[512];
		size_t used = 0;
		time_t connected_at = time(NULL), last_query = connected_at;
		if (fd < 0) {
			fprintf(stderr, "AVR connect to %s:%u failed\n", app->avr_host,
					app->avr_port);
			sleep(2);
			continue;
		}
		fprintf(stderr, "connected to AVR control endpoint\n");
		if (send_command(fd, "MV?\rMU?\r") < 0) {
			close(fd);
			continue;
		}
		while (!network_should_stop(app)) {
			struct pollfd pfd = { .fd = fd, .events = POLLIN };
			int timeout = 100;
			int polled = poll(&pfd, 1, timeout);
			if (polled < 0 && errno != EINTR)
				break;
			if (polled > 0 && (pfd.revents & (POLLERR | POLLHUP | POLLNVAL)))
				break;
			if (polled > 0 && (pfd.revents & POLLIN)) {
				char incoming[256];
				ssize_t count = recv(fd, incoming, sizeof(incoming), 0);
				ssize_t i;
				if (count <= 0)
					break;
				for (i = 0; i < count; i++) {
					char c = incoming[i];
					if (c == '\r' || c == '\n') {
						if (used > 0) {
							buffer[used] = '\0';
							uint32_t changed = denon_parse_line(buffer, &status);
							if (changed && ready)
								dispatch_avr_status(app, &status);
							used = 0;
							if (!ready && (status.seen & (DENON_UPDATE_VOLUME |
									DENON_UPDATE_MAX | DENON_UPDATE_MUTE)) ==
								(DENON_UPDATE_VOLUME | DENON_UPDATE_MAX |
								DENON_UPDATE_MUTE)) {
								ready = true;
								last_code = (int)lround(status.volume_code);
								last_mute = status.mute;
								dispatch_avr_status(app, &status);
							}
						}
					} else if (used + 1 < sizeof(buffer)) {
						buffer[used++] = c;
					}
				}
			}
			if (!ready && time(NULL) - connected_at >= 1 &&
					(status.seen & (DENON_UPDATE_VOLUME | DENON_UPDATE_MAX)) ==
					(DENON_UPDATE_VOLUME | DENON_UPDATE_MAX)) {
				ready = true;
				last_code = (int)lround(status.volume_code);
				last_mute = (status.seen & DENON_UPDATE_MUTE) ? status.mute : 0;
				dispatch_avr_status(app, &status);
			}
			if (ready) {
				int wanted_code, wanted_mute;
				bool network_ready, write_enabled;
				pthread_mutex_lock(&app->network_lock);
				network_ready = app->network_ready;
				write_enabled = app->write_enabled;
				wanted_code = app->desired_code;
				wanted_mute = app->desired_mute;
				pthread_mutex_unlock(&app->network_lock);
				if (network_ready && write_enabled && wanted_code != last_code) {
					char command[16];
					if (wanted_code < 0)
						wanted_code = 0;
					if (wanted_code > (int)floor(status.max_code))
						wanted_code = (int)floor(status.max_code);
					snprintf(command, sizeof(command), "MV%02d\r", wanted_code);
					if (send_command(fd, command) < 0)
						break;
					fprintf(stderr, "sent AVR command %s", command);
					last_code = wanted_code;
				}
				if (network_ready && write_enabled && wanted_mute != last_mute) {
					if (send_command(fd, wanted_mute ? "MUON\r" : "MUOFF\r") < 0)
						break;
					fprintf(stderr, "sent AVR mute %s\n", wanted_mute ? "on" : "off");
					last_mute = wanted_mute;
				}
			}
			if (time(NULL) - last_query >= 30) {
				if (send_command(fd, "MV?\rMU?\r") < 0)
					break;
				last_query = time(NULL);
			}
		}
		close(fd);
		pthread_mutex_lock(&app->network_lock);
		app->network_ready = false;
		pthread_mutex_unlock(&app->network_lock);
		if (!network_should_stop(app))
			sleep(2);
	}
	return NULL;
}

static void on_signal(void *userdata, int signal_number)
{
	struct app *app = userdata;
	(void)signal_number;
	if (app->network_enabled) {
		pthread_mutex_lock(&app->network_lock);
		app->stop_network = true;
		pthread_mutex_unlock(&app->network_lock);
	}
	pw_main_loop_quit(app->loop);
}

static const struct pw_filter_events filter_events = {
	.version = PW_VERSION_FILTER_EVENTS,
	.param_changed = on_param_changed,
	.process = on_process,
};

static int make_links(struct app *app)
{
	uint32_t i;
	for (i = 0; i < CHANNELS; i++) {
		char output[320];
		char input[320];
		pid_t pid;

		snprintf(output, sizeof(output), "%s:output_%s", app->node_name,
				channel_names[i]);
		snprintf(input, sizeof(input), "%s:playback_%s", app->target_sink,
				channel_names[i]);
		pid = fork();
		if (pid == 0) {
			execlp("pw-link", "pw-link", "-L", "-w", output, input,
					(char *)NULL);
			_exit(127);
		}
		if (pid < 0) {
			perror("fork pw-link");
			return -1;
		}
	}
	return 0;
}

static int default_target(char *target, size_t target_size)
{
	FILE *pipe = popen("pactl get-default-sink", "r");
	size_t length;
	bool read_ok;
	int status;
	if (pipe == NULL)
		return -1;
	read_ok = fgets(target, (int)target_size, pipe) != NULL;
	status = pclose(pipe);
	if (!read_ok || status != 0)
		return -1;
	length = strcspn(target, "\r\n");
	target[length] = '\0';
	return length == 0 ? -1 : 0;
}

int main(int argc, char *argv[])
{
	struct app app = {
		.volume = 0.216f,
		.max_code = 98,
		.avr_port = 23,
		.network_enabled = true,
		.write_enabled = true,
	};
	const struct spa_pod *params[4];
	uint8_t pod_buffer[4096];
	struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(pod_buffer,
			sizeof(pod_buffer));
	uint32_t n_params = 0;
	uint32_t i;
	int result = EXIT_FAILURE;
	bool target_explicit = false;
	bool lock_initialized = false;
	snprintf(app.node_name, sizeof(app.node_name), "avr_volume_agent");
	snprintf(app.avr_host, sizeof(app.avr_host), "10.1.92.186");

	for (i = 1; i < (uint32_t)argc; i++) {
		if (strcmp(argv[i], "--target-sink") == 0 && i + 1 < (uint32_t)argc) {
			snprintf(app.target_sink, sizeof(app.target_sink), "%s", argv[++i]);
			target_explicit = true;
		} else if (strcmp(argv[i], "--node-name") == 0 && i + 1 < (uint32_t)argc) {
			snprintf(app.node_name, sizeof(app.node_name), "%s", argv[++i]);
		} else if (strcmp(argv[i], "--avr-host") == 0 && i + 1 < (uint32_t)argc) {
			snprintf(app.avr_host, sizeof(app.avr_host), "%s", argv[++i]);
		} else if (strcmp(argv[i], "--port") == 0 && i + 1 < (uint32_t)argc) {
			unsigned long port = strtoul(argv[++i], NULL, 10);
			if (port == 0 || port > 65535) {
				fprintf(stderr, "invalid TCP port\n");
				return EXIT_FAILURE;
			}
			app.avr_port = (uint16_t)port;
		} else if (strcmp(argv[i], "--no-network") == 0) {
			app.network_enabled = false;
		} else if (strcmp(argv[i], "--read-only") == 0) {
			app.write_enabled = false;
		} else if (strcmp(argv[i], "--help") == 0) {
			printf("Usage: %s [--target-sink PIPEWIRE_SINK] [--avr-host HOST] "
					"[--port TCP_PORT] [--node-name NAME] [--no-network] "
					"[--read-only]\n", argv[0]);
			return EXIT_SUCCESS;
		} else {
			fprintf(stderr, "unknown argument: %s\n", argv[i]);
			return EXIT_FAILURE;
		}
	}
	if (!target_explicit && default_target(app.target_sink,
			sizeof(app.target_sink)) < 0) {
		fprintf(stderr, "could not determine default HDMI sink; use --target-sink\n");
		return EXIT_FAILURE;
	}
	if (strcmp(app.target_sink, app.node_name) == 0) {
		fprintf(stderr, "target sink cannot be the virtual sink itself\n");
		return EXIT_FAILURE;
	}
	if (pthread_mutex_init(&app.network_lock, NULL) != 0) {
		fprintf(stderr, "could not initialize network lock\n");
		return EXIT_FAILURE;
	}
	lock_initialized = true;

	pw_init(&argc, &argv);
	app.loop = pw_main_loop_new(NULL);
	if (app.loop == NULL) {
		fprintf(stderr, "failed to create PipeWire main loop\n");
		goto done;
	}
	app.filter = pw_filter_new_simple(pw_main_loop_get_loop(app.loop),
			"avr-volume-agent",
			pw_properties_new(
				PW_KEY_MEDIA_TYPE, "Audio",
				PW_KEY_MEDIA_CATEGORY, "Playback",
				PW_KEY_MEDIA_ROLE, "Music",
				PW_KEY_MEDIA_CLASS, "Audio/Sink",
				PW_KEY_NODE_NAME, app.node_name,
				PW_KEY_NODE_DESCRIPTION, "AVR HDMI (network volume)",
				PW_KEY_NODE_NICK, "AVR HDMI",
				PW_KEY_NODE_VIRTUAL, "true",
				PW_KEY_NODE_AUTOCONNECT, "true",
				PW_KEY_NODE_DRIVER, "true",
				PW_KEY_PRIORITY_SESSION, "2000",
				PW_KEY_AUDIO_CHANNELS, "6",
				"audio.position", "[ FL, FR, RL, RR, FC, LFE ]",
				NULL),
			&filter_events, &app);
	if (app.filter == NULL) {
		fprintf(stderr, "failed to create PipeWire filter\n");
		goto done;
	}

	for (i = 0; i < CHANNELS; i++) {
		char input_name[32];
		char output_name[32];
		const struct spa_pod *port_params[1];
		uint8_t port_buffer[512];
		struct spa_pod_builder port_builder = SPA_POD_BUILDER_INIT(port_buffer,
				sizeof(port_buffer));
		uint32_t position = channel_ids[i];

		snprintf(input_name, sizeof(input_name), "input_%s", channel_names[i]);
		snprintf(output_name, sizeof(output_name), "output_%s", channel_names[i]);
		port_params[0] = build_audio_format(&port_builder, 1, &position);
		app.input[i] = pw_filter_add_port(app.filter, PW_DIRECTION_INPUT,
				PW_FILTER_PORT_FLAG_MAP_BUFFERS, sizeof(struct port),
				pw_properties_new(PW_KEY_FORMAT_DSP, "32 bit float mono audio",
					PW_KEY_PORT_NAME, input_name,
					PW_KEY_PORT_GROUP, "playback",
					PW_KEY_AUDIO_CHANNEL, channel_names[i], NULL),
				port_params, 1);
		app.output[i] = pw_filter_add_port(app.filter, PW_DIRECTION_OUTPUT,
				PW_FILTER_PORT_FLAG_MAP_BUFFERS, sizeof(struct port),
				pw_properties_new(PW_KEY_FORMAT_DSP, "32 bit float mono audio",
					PW_KEY_PORT_NAME, output_name,
					PW_KEY_PORT_GROUP, "playback",
					PW_KEY_AUDIO_CHANNEL, channel_names[i], NULL),
				port_params, 1);
		if (app.input[i] == NULL || app.output[i] == NULL) {
			fprintf(stderr, "failed to create audio ports\n");
			goto done;
		}
		app.input[i]->app = &app;
		app.output[i]->app = &app;
	}

	params[n_params++] = build_audio_format(&builder, CHANNELS, channel_ids);
	params[n_params++] = build_props(&builder, app.volume, app.mute);
	params[n_params++] = build_volume_info(&builder);
	params[n_params++] = build_mute_info(&builder);
	if (pw_filter_connect(app.filter, PW_FILTER_FLAG_RT_PROCESS, params,
			n_params) < 0) {
		fprintf(stderr, "failed to connect PipeWire filter\n");
		goto done;
	}
	pw_filter_set_active(app.filter, true);
	signal(SIGCHLD, SIG_IGN);
	if (make_links(&app) < 0)
		goto done;

	pw_loop_add_signal(pw_main_loop_get_loop(app.loop), SIGINT, on_signal, &app);
	pw_loop_add_signal(pw_main_loop_get_loop(app.loop), SIGTERM, on_signal, &app);
	if (app.network_enabled) {
		if (pthread_create(&app.network_thread, NULL, network_worker, &app) != 0) {
			fprintf(stderr, "could not start AVR network worker\n");
			goto done;
		}
		app.network_started = true;
	}
	fprintf(stderr, "created unity-gain sink '%s' targeting '%s'%s\n",
			app.node_name, app.target_sink,
			app.network_enabled ? " with AVR control" : " without AVR control");
	pw_main_loop_run(app.loop);
	result = EXIT_SUCCESS;

done:
	if (app.network_started) {
		pthread_mutex_lock(&app.network_lock);
		app.stop_network = true;
		pthread_mutex_unlock(&app.network_lock);
		pthread_join(app.network_thread, NULL);
	}
	if (app.filter != NULL)
		pw_filter_destroy(app.filter);
	if (app.loop != NULL)
		pw_main_loop_destroy(app.loop);
	if (lock_initialized)
		pthread_mutex_destroy(&app.network_lock);
	pw_deinit();
	return result;
}
