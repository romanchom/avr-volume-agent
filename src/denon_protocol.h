#ifndef AVR_VOLUME_DENON_PROTOCOL_H
#define AVR_VOLUME_DENON_PROTOCOL_H

#include <stdbool.h>
#include <stdint.h>

#define DENON_UPDATE_VOLUME (1u << 0)
#define DENON_UPDATE_MAX (1u << 1)
#define DENON_UPDATE_MUTE (1u << 2)

struct denon_status {
	double max_code;
	double volume_code;
	bool mute;
	uint32_t seen;
};

uint32_t denon_parse_line(const char *line, struct denon_status *status);
int denon_slider_to_code(float channel_volume, double max_code);
float denon_code_to_channel_volume(double volume_code, double max_code);

#endif
